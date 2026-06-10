use std::cmp::Ordering;
use std::collections::HashMap;

use kimetsu_core::config::{BrokerWeights, StageWeights};
use kimetsu_core::memory::MemoryScope;
use kimetsu_core::{KimetsuResult, ids::new_id};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// E3: task-kind classification + adaptive retrieval routing
// -----------------------------------------------------------------------

/// The inferred kind of the current coding task. Classified once at
/// intake from the task description string — deterministic keyword
/// scan, no model call, zero allocation-heavy work.
///
/// `Feature` is the NEUTRAL default: it does not change weights or
/// prefer_roles at all, so every existing `..Default::default()`
/// construction produces exactly the prior retrieval behaviour.
///
/// Precedence when multiple keyword sets match:
///   Debug > Investigation > Refactor > Docs > Feature
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskKind {
    /// Neutral / catch-all (add, implement, build, create, support, …).
    /// Must NOT alter weights or prefer_roles — keeps existing tests green.
    #[default]
    Feature,
    /// fix, bug, error, fail, crash, panic, regression, broken, debug,
    /// stack trace, exception — up freshness, prefer failure_pattern.
    Debug,
    /// refactor, rename, cleanup, restructure, simplify, extract,
    /// deduplicate, reorganize — up scope, prefer convention.
    Refactor,
    /// document, readme, changelog, comment, docstring, docs, tutorial,
    /// guide — near-neutral mild adjustments.
    Docs,
    /// investigate, analyze, understand, why, explore, find out,
    /// root cause, audit, trace — up relevance, prefer fact + preference.
    Investigation,
}

/// Classify a task description string into a [`TaskKind`] using a
/// deterministic keyword scan over the lowercased text. No model call.
///
/// Precedence (highest wins when multiple sets match):
///   Debug > Investigation > Refactor > Docs > Feature
pub fn classify_task(task: &str) -> TaskKind {
    let lower = task.to_ascii_lowercase();

    // Debug keywords (highest priority)
    const DEBUG_KW: &[&str] = &[
        "fix",
        "bug",
        "error",
        "fail",
        "crash",
        "panic",
        "regression",
        "broken",
        "debug",
        "stack trace",
        "exception",
    ];
    if DEBUG_KW.iter().any(|kw| lower.contains(kw)) {
        return TaskKind::Debug;
    }

    // Investigation keywords
    const INVESTIGATE_KW: &[&str] = &[
        "investigate",
        "analyze",
        "understand",
        " why ",
        "explore",
        "find out",
        "root cause",
        "audit",
        "trace",
    ];
    if INVESTIGATE_KW.iter().any(|kw| lower.contains(kw)) {
        return TaskKind::Investigation;
    }

    // Refactor keywords
    const REFACTOR_KW: &[&str] = &[
        "refactor",
        "rename",
        "cleanup",
        "clean up",
        "restructure",
        "simplify",
        "extract",
        "deduplicate",
        "reorganize",
    ];
    if REFACTOR_KW.iter().any(|kw| lower.contains(kw)) {
        return TaskKind::Refactor;
    }

    // Docs keywords
    const DOCS_KW: &[&str] = &[
        "document",
        "readme",
        "changelog",
        "comment",
        "docstring",
        "docs",
        "tutorial",
        "guide",
    ];
    if DOCS_KW.iter().any(|kw| lower.contains(kw)) {
        return TaskKind::Docs;
    }

    // Default: Feature (neutral)
    TaskKind::Feature
}

/// Compose task-kind weight biases on top of the stage weights.
///
/// For `Feature`, returns `base` UNCHANGED — this is the neutrality
/// guarantee that keeps all existing retrieval tests green.
///
/// For other kinds, one component is multiplied by a bias factor and
/// the result is renormalized so the four weights still sum to the same
/// total as `base`, preserving overall scoring magnitude (just the mix
/// changes).
///
/// Bias factors (applied before renorm):
/// - Debug       → freshness × 1.6  (recent failures matter most)
/// - Refactor    → scope × 1.6      (project/repo conventions matter most)
/// - Investigation → relevance × 1.4 (broad fact/preference recall)
/// - Docs        → mild (confidence × 1.15, near-neutral)
fn weights_for_task_kind(base: StageWeights, kind: TaskKind) -> StageWeights {
    match kind {
        TaskKind::Feature => base,
        TaskKind::Debug => renorm(StageWeights {
            freshness: base.freshness * 1.6,
            ..base
        }),
        TaskKind::Refactor => renorm(StageWeights {
            scope: base.scope * 1.6,
            ..base
        }),
        TaskKind::Investigation => renorm(StageWeights {
            relevance: base.relevance * 1.4,
            ..base
        }),
        TaskKind::Docs => renorm(StageWeights {
            confidence: base.confidence * 1.15,
            ..base
        }),
    }
}

/// Renormalize `StageWeights` so the four components sum to the same
/// total as before the bias was applied. This preserves scoring
/// magnitude — only the mix changes.
fn renorm(w: StageWeights) -> StageWeights {
    let sum = w.relevance + w.confidence + w.freshness + w.scope;
    if sum <= f32::EPSILON {
        return w;
    }
    // The original sum (before any bias) isn't available here; instead
    // we scale to 1.0 and then the absolute scores are comparable
    // because normalize_and_score already places components in [0,1].
    // NOTE: the stage weights themselves don't need to sum to 1.0 —
    // the existing defaults (0.5+0.2+0.2+0.1=1.0) do, but the
    // renormalization target should be the unbiased sum so we don't
    // change the overall scale. Since we only modify ONE component by a
    // small factor, we scale back to 1.0 (the natural target).
    StageWeights {
        relevance: w.relevance / sum,
        confidence: w.confidence / sum,
        freshness: w.freshness / sum,
        scope: w.scope / sum,
    }
}

/// Return the additional `prefer_roles` hints implied by `kind`.
///
/// These are MERGED with any caller-supplied `prefer_roles` (not
/// clobbered), so the task-kind bias is additive.
/// For `Feature`, returns an empty slice — zero effect on existing behaviour.
fn task_kind_prefer_roles(kind: TaskKind) -> &'static [&'static str] {
    match kind {
        TaskKind::Feature => &[],
        TaskKind::Debug => &["failure_pattern"],
        TaskKind::Refactor => &["convention"],
        TaskKind::Investigation => &["fact", "preference"],
        TaskKind::Docs => &["convention"],
    }
}
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

impl ContextCapsule {
    /// v1.0.0: build a render-only capsule from daemon wire data. Only the
    /// fields the hook renders (`summary`, `kind`, `score`) are meaningful;
    /// the rest are zeroed — this capsule is never re-scored or expanded.
    pub fn wire_minimal(summary: String, kind: String, score: f32) -> Self {
        Self {
            id: String::new(),
            kind,
            summary,
            token_estimate: 0,
            expansion_handle: String::new(),
            provenance: Vec::new(),
            confidence: 0.0,
            freshness: 0.0,
            relevance: 0.0,
            scope_weight: 0.0,
            score,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceRef {
    pub source: String,
    pub id: String,
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ContextRequest {
    pub stage: String,
    pub query: String,
    pub budget_tokens: u32,
    /// v0.6: domain-hint tags. Capsules whose text or kind contains any
    /// of these strings receive a 1.4× score boost, pushing on-domain
    /// capsules above the `min_score` threshold when they would otherwise
    /// be filtered out.
    pub tags: Vec<String>,
    /// v0.6: minimum composite score for inclusion. When > 0.0 and the
    /// top-scoring capsule falls below this threshold, `ContextBundle`
    /// is returned with `skipped: true` and an empty capsule list —
    /// zero tokens injected. 0.0 (default) disables the check.
    pub min_score: f32,
    /// v0.6: hard cap on returned capsules regardless of token budget.
    /// 0 = no cap (budget-only limit, prior behaviour).
    pub max_capsules: usize,
    /// v0.6: role-preference boost. Capsules whose `kind` matches one
    /// of these strings receive an additional 1.3× multiplier after the
    /// tag boost (e.g. `["semantic_operator", "anti_pattern"]` for bench).
    pub prefer_roles: Vec<String>,
    /// v0.8: hard kind filter applied BEFORE scoring + capping. When
    /// non-empty, only candidates whose capsule `kind` is in this list
    /// survive — so a higher-ranked repo file or off-kind memory can't
    /// consume a (often single) slot. Used by the proactive engine to
    /// restrict recall to actionable kinds (failure_pattern, command,
    /// convention). Empty (default) keeps all kinds, prior behaviour.
    pub kinds: Vec<String>,
    /// D1e: absolute cosine-similarity floor. On embeddings builds,
    /// memory candidates whose cosine to the query is below this
    /// threshold are dropped before budgeting. 0.0 (default) disables
    /// the floor — matches pre-D1e behaviour. Repo-file and manifest
    /// candidates are unaffected (they have no cosine score). Populated
    /// from `BrokerSection.min_semantic_score` by the pipeline; callers
    /// that don't set it get the prior behaviour automatically.
    pub min_semantic_score: f32,
    /// v1.0.0: absolute *lexical* relevance floor for memory candidates,
    /// as the fraction of the query's IDF-weighted discriminating power a
    /// memory must cover. Unlike `min_semantic_score` this needs no query
    /// embedding, so it protects the FTS-only hook path. When > 0.0, memory
    /// candidates below the floor are dropped BEFORE scoring (so they don't
    /// even set the per-kind normalization max). Repo-file/manifest
    /// candidates are unaffected. 0.0 (default) disables it — every existing
    /// `..Default::default()` construction is unchanged. Populated from
    /// `BrokerSection.min_lexical_coverage` by the pipeline.
    pub min_lexical_coverage: f32,
    /// E3: inferred kind of the current task. Defaults to `Feature`
    /// (the neutral kind) so every existing `..Default::default()`
    /// construction is unchanged — Feature does NOT alter weights or
    /// prefer_roles. Set by the pipeline via `classify_task` at intake.
    pub task_kind: TaskKind,
}

#[derive(Debug, Clone)]
pub struct ContextBundle {
    pub stage: String,
    pub budget_tokens: u32,
    pub used_tokens: u32,
    pub capsules: Vec<ContextCapsule>,
    pub excluded: Vec<ContextCapsule>,
    /// v0.6: true when the top capsule score was below `min_score`.
    /// All capsules are empty; no tokens were injected.
    pub skipped: bool,
    /// v0.6: best composite score observed before the skip check.
    /// Useful for diagnostics ("why was the brain silent?").
    pub top_score: f32,
}

#[derive(Debug, Clone)]
struct Candidate {
    capsule: ContextCapsule,
    raw_relevance: f32,
    /// D1e: the row's embedding vector, present when the row's
    /// `embedding_model` matches the active query embedder's id.
    /// `None` for repo-file/manifest candidates and for memory rows
    /// whose model differs from the active embedder (cross-model
    /// rows). Used by the candidate-stage embedding-MMR pass.
    embedding: Option<Vec<f32>>,
    /// D1e: raw cosine similarity between this candidate and the
    /// query embedding. Present when `embedding` is `Some`. Used for
    /// the absolute semantic relevance floor (min_semantic_score).
    cosine: Option<f32>,
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
        embedder,
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
    let half_life_days = weights.decay_half_life_days;
    let mut candidates = Vec::new();
    candidates.extend(memory_candidates(
        conn,
        &request.query,
        query_embedding.as_ref(),
        half_life_days,
    )?);
    for extra in extra_memory_conns {
        candidates.extend(memory_candidates(
            extra,
            &request.query,
            query_embedding.as_ref(),
            half_life_days,
        )?);
    }
    candidates.extend(repo_file_candidates(conn, repo_root, &request.query, 30)?);
    candidates.extend(manifest_candidates(conn, repo_root, &request.query)?);

    // v0.8: proactive kind filter — restrict to actionable kinds BEFORE
    // scoring + capping so a higher-ranked repo file or off-kind memory
    // can't take the proactive slot and get filtered out afterwards.
    // Memory capsules carry the generic `kind: "memory"` and encode the
    // real memory kind in the summary prefix ("scope:kind - text"), so
    // match against that for memories.
    if !request.kinds.is_empty() {
        candidates.retain(|c| {
            request
                .kinds
                .iter()
                .any(|k| capsule_matches_kind(&c.capsule, k))
        });
    }

    // v1.0.0: absolute LEXICAL relevance floor. The FTS-only hook path has
    // no cosine, so the `min_semantic_score` floor below can't protect it —
    // a broad conceptual query whose only matching tokens are corpus-
    // ubiquitous (e.g. the project name) would otherwise surface unrelated
    // memories, which per-kind normalization later promotes to relevance=1.0
    // regardless of how weak the match is.
    //
    // We compute an IDF-weighted coverage in [0,1] over the query's CONTENT
    // tokens (stopwords removed; ubiquitous tokens carry ~0 IDF so they don't
    // drive coverage) and drop a memory candidate when its coverage is below
    // the floor AND it has no semantic support. Applied BEFORE scoring so
    // pruned rows don't even set the per-kind normalization max. Only memory
    // candidates are floored — repo_file/manifest capsules pass through (an
    // FTS match on file content is itself a relevance signal, and overview
    // queries *want* the README). Inert when the floor is 0.0 or the query
    // has no discriminating (non-ubiquitous) content token.
    if request.min_lexical_coverage > 0.0 {
        let content = content_tokens(&request.query);
        if !content.is_empty() {
            let idf = corpus_token_idf(conn, &content)?;
            let total_idf: f32 = content
                .iter()
                .map(|t| idf.get(t).copied().unwrap_or(0.0))
                .sum();
            // Skip the floor when no content token is discriminating — every
            // token is corpus-ubiquitous, so we have no signal to floor on.
            if total_idf > f32::EPSILON {
                candidates.retain(|c| {
                    if c.capsule.kind != "memory" {
                        return true; // repo_file / manifest pass through
                    }
                    // Semantic support keeps a lexically-thin but on-topic
                    // memory on embeddings builds (cosine is None on the hook).
                    if c.cosine.is_some_and(|cos| cos >= SEMANTIC_KEEP_COSINE) {
                        return true;
                    }
                    weighted_coverage(&content, &idf, &c.capsule.summary)
                        >= request.min_lexical_coverage
                });
            }
        }
    }

    // E3: compose task-kind weight bias over stage weights, then renormalize.
    // For Feature (default), weights_for_task_kind returns the base unchanged.
    let stage_weights = weights_for_stage(weights, &request.stage);
    let effective_weights = weights_for_task_kind(stage_weights, request.task_kind);
    normalize_and_score(&mut candidates, effective_weights);

    // E3: merge task-kind prefer_role hints with caller-supplied prefer_roles.
    // For Feature the hints are empty so this is a no-op (neutral).
    let kind_role_hints = task_kind_prefer_roles(request.task_kind);
    let mut effective_prefer_roles: Vec<String> = request.prefer_roles.clone();
    for &hint in kind_role_hints {
        let hint_s = hint.to_string();
        if !effective_prefer_roles.contains(&hint_s) {
            effective_prefer_roles.push(hint_s);
        }
    }

    // v0.6: apply tag boost (1.4×) and role-preference boost (1.3×) after
    // normalisation so the multipliers operate on the [0,1]-normalised score
    // rather than the raw pre-normalisation values.
    //
    // E3: the role-preference check uses `capsule_matches_kind` so that
    // memory capsules (whose outer `kind` is always `"memory"`) are matched
    // against the real sub-kind embedded in their summary prefix
    // (`"scope:kind - text"`). This makes task-kind prefer_role hints
    // (e.g. "failure_pattern" for Debug) actually work for memory capsules.
    // For non-memory capsules (repo_file, manifest) the outer kind is checked
    // directly — same behaviour as before for caller-supplied prefer_roles.
    if !request.tags.is_empty() || !effective_prefer_roles.is_empty() {
        let tags_lc: Vec<String> = request
            .tags
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        for c in &mut candidates {
            let summary_lc = c.capsule.summary.to_ascii_lowercase();
            if !tags_lc.is_empty() && tags_lc.iter().any(|t| summary_lc.contains(t.as_str())) {
                c.capsule.score *= 1.4;
            }
            if !effective_prefer_roles.is_empty()
                && effective_prefer_roles.iter().any(|r| {
                    // For memory capsules: check the real sub-kind embedded in the
                    // summary prefix ("scope:kind - text") via capsule_matches_kind.
                    // This makes task-kind prefer_role hints work for memory capsules
                    // whose outer `kind` field is always the generic "memory" string.
                    // For non-memory capsules: fall back to the original substring
                    // check on the outer `kind` field (preserves v0.6 behaviour for
                    // caller-supplied prefer_roles like "semantic_operator").
                    if c.capsule.kind == "memory" {
                        capsule_matches_kind(&c.capsule, r.as_str())
                    } else {
                        c.capsule.kind.contains(r.as_str())
                    }
                })
            {
                c.capsule.score *= 1.3;
            }
        }
    }

    // D1e-2: absolute semantic relevance floor. On embeddings builds
    // (query_embedding is Some), drop candidates whose cosine to the
    // query is strictly below min_semantic_score. This ensures a
    // genuinely-irrelevant corpus hits the zero-capsule skipped path
    // rather than surfacing its "best of a bad lot". Inert on lean
    // builds (query_embedding is None) or when floor is 0.0.
    //
    // Applied BEFORE the candidate→capsule conversion so irrelevant
    // rows don't consume budget or affect normalization.
    //
    // Only applied to memory candidates (those with cosine populated);
    // repo_file and manifest candidates have cosine=None and are
    // always passed through — they're matched by FTS which is already
    // a signal of relevance.
    if query_embedding.is_some() && request.min_semantic_score > 0.0 {
        candidates.retain(|c| {
            // Keep non-memory candidates (no cosine) and memory
            // candidates that cleared the floor.
            match c.cosine {
                Some(cos) => cos >= request.min_semantic_score,
                None => true,
            }
        });
    }

    // D1e-1: candidate-stage embedding-MMR. On embeddings builds,
    // apply MMR over the ranked Vec<Candidate> using cosine similarity
    // between candidate embeddings as the redundancy measure. This
    // collapses true semantic near-duplicates ("prefer rg over grep"
    // and "use ripgrep") that Jaccard-of-tokens would miss.
    //
    // When EITHER candidate lacks an embedding (repo-file, manifest,
    // or a cross-model memory row), falls back to Jaccard similarity
    // of summary tokens — the same measure the existing capsule-stage
    // MMR uses. This preserves lean parity exactly.
    //
    // Sort by score descending first so the greedy MMR seeds on the
    // top-scoring candidate (same as the capsule-stage MMR).
    candidates.sort_by(|a, b| {
        b.capsule
            .score
            .partial_cmp(&a.capsule.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.capsule
                    .freshness
                    .partial_cmp(&a.capsule.freshness)
                    .unwrap_or(Ordering::Equal)
            })
            // Deterministic tiebreak on the STABLE handle (memory:<id> /
            // file:<path>) — capsule.id is a fresh random ULID per retrieval,
            // so tiebreaking on it would make retrieval non-reproducible on
            // score+freshness ties.
            .then_with(|| a.capsule.expansion_handle.cmp(&b.capsule.expansion_handle))
    });

    // Run embedding-MMR on embeddings builds; lean builds skip directly
    // to the capsule-stage Jaccard MMR below.
    let embedding_mmr_ran = query_embedding.is_some() && !candidates.is_empty();
    let candidates = if embedding_mmr_ran {
        apply_candidate_mmr_diversity(candidates, 0.7)
    } else {
        candidates
    };

    let mut capsules = candidates
        .into_iter()
        .map(|candidate| candidate.capsule)
        .collect::<Vec<_>>();

    // After embedding-MMR the candidate list is already in MMR order.
    // On lean builds (no embedding-MMR) we still need to sort by score.
    if !embedding_mmr_ran {
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
                // Stable handle tiebreak (capsule.id is random per retrieval).
                .then_with(|| left.expansion_handle.cmp(&right.expansion_handle))
        });
    }

    // v0.6: confidence-aware skip — if the top score is below the caller's
    // threshold, return an empty bundle immediately. Zero tokens injected.
    let top_score = capsules.first().map(|c| c.score).unwrap_or(0.0);
    if request.min_score > 0.0 && top_score < request.min_score {
        return Ok(ContextBundle {
            stage: request.stage,
            budget_tokens: request.budget_tokens,
            used_tokens: 0,
            capsules: Vec::new(),
            excluded: capsules,
            skipped: true,
            top_score,
        });
    }

    // MP-17 #13: capsule-stage Jaccard MMR — safety net / lean path.
    // On embeddings builds the candidate-stage embedding-MMR already
    // collapsed semantic near-duplicates; this pass is largely a no-op
    // (same-kind Jaccard score will be low for already-deduped summaries)
    // but provides a final guard against any remaining token-level
    // duplicates (e.g. repo files with heavily overlapping snippets).
    // On lean builds this is the sole diversity mechanism (unchanged).
    let capsules = apply_mmr_diversity(capsules, 0.7);

    let capsule_budget = request.budget_tokens / 2;
    let mut used_tokens = 0u32;
    let mut included = Vec::new();
    let mut excluded = Vec::new();

    for capsule in capsules {
        // v0.6: max_capsules cap (0 = disabled)
        if request.max_capsules > 0 && included.len() >= request.max_capsules {
            excluded.push(capsule);
            continue;
        }
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
        skipped: false,
        top_score,
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

// -----------------------------------------------------------------------
// ANN candidate generation via the usearch HNSW index — embeddings only.
// (The old brute-force `vec0` index code was removed in T3c; usearch now
// supersedes it entirely. See `crate::ann`.)
// -----------------------------------------------------------------------

/// Top-K ANN candidates from the usearch HNSW index.
///
/// Returns memory rows fetched from `memories` (same columns as
/// `latest_memory_candidates`) built into `Candidate`s via
/// `memory_row_to_candidate`. Callers union this with the FTS set and dedup.
#[cfg(feature = "embeddings")]
fn memory_ann_candidates(
    conn: &Connection,
    qe: &QueryEmbedding,
    k: u32,
    query_tokens: &[String],
    half_life_days: f32,
) -> KimetsuResult<Vec<Candidate>> {
    // Tier-3: ANN candidate generation via the usearch HNSW index.
    let handle = crate::ann::handle_for_query(conn, qe.vector.len(), &qe.model_id)?;
    let hits = handle
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .search(&qe.vector, k as usize)?;
    // Map rowids back to memory_ids (active-only is enforced by the index, but
    // we still join `memories` below for the full row + the embedding_model
    // residual filter, so collect rowids here).
    let knn_rowids: Vec<i64> = hits.into_iter().map(|(rowid, _dist)| rowid).collect();
    if knn_rowids.is_empty() {
        return Ok(Vec::new());
    }

    // Fetch full memory rows for those rowids (same projection as latest_memory_candidates).
    let placeholders: String = knn_rowids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT memory_id, scope, kind, text, confidence, created_at,
                use_count, usefulness_score, embedding, embedding_model,
                last_useful_at
         FROM   memories
         WHERE  invalidated_at IS NULL
           AND  embedding_model = ?{model_param}
           AND  rowid IN ({placeholders})",
        model_param = knn_rowids.len() + 1
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params_vec: Vec<&dyn rusqlite::ToSql> = knn_rowids
        .iter()
        .map(|n| n as &dyn rusqlite::ToSql)
        .collect();
    params_vec.push(&qe.model_id);
    let rows_iter = stmt.query_map(params_vec.as_slice(), |row| {
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
            row.get::<_, Option<String>>(10)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows_iter {
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
            last_useful_at,
        ) = row?;
        let (cosine, row_vec) =
            compute_cosine_and_vec(Some(qe), embedding.as_deref(), embedding_model.as_deref());
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
            last_useful_at,
            half_life_days,
            None, // no raw FTS relevance override — cosine drives ranking
            cosine,
            row_vec,
        ) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn memory_candidates(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&QueryEmbedding>,
    half_life_days: f32,
) -> KimetsuResult<Vec<Candidate>> {
    let query_tokens = query_tokens(query);

    // D1c: on embeddings builds with a real query vector, run BOTH FTS and
    // ANN, then union-dedup by memory_id (keeping the higher-scored instance).
    // This replaces the recency-bounded latest_memory_candidates fallback as
    // the semantic-recall source when embeddings are active.
    #[cfg(feature = "embeddings")]
    if let Some(qe) = query_embedding {
        // FTS candidates (may be empty if no lexical matches).
        let fts_candidates = if let Some(fts_query) = fts_query(query) {
            memory_fts_candidates(
                conn,
                &query_tokens,
                &fts_query,
                80,
                Some(qe),
                half_life_days,
            )?
        } else {
            Vec::new()
        };

        // ANN candidates — top-80 nearest neighbours from the usearch index.
        let ann_candidates = memory_ann_candidates(conn, qe, 80, &query_tokens, half_life_days)?;

        // Union the two sets, deduped by memory_id.  When a memory appears
        // in both, keep the instance with the higher raw_relevance so
        // candidates that both lexically and semantically match the query
        // get the best score.
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut merged: Vec<Candidate> = Vec::new();

        for candidate in fts_candidates.into_iter().chain(ann_candidates) {
            // Extract memory_id from the expansion_handle "memory:<id>".
            let mid = candidate
                .capsule
                .expansion_handle
                .strip_prefix("memory:")
                .unwrap_or(&candidate.capsule.expansion_handle)
                .to_string();
            if let Some(&idx) = seen.get(&mid) {
                // Keep the higher-scored instance.
                if candidate.raw_relevance > merged[idx].raw_relevance {
                    merged[idx] = candidate;
                }
            } else {
                seen.insert(mid, merged.len());
                merged.push(candidate);
            }
        }

        return Ok(merged);
    }

    // Lean (NoopEmbedder) path: unchanged — FTS then recency fallback.
    if let Some(fts_query) = fts_query(query) {
        let candidates = memory_fts_candidates(
            conn,
            &query_tokens,
            &fts_query,
            80,
            query_embedding,
            half_life_days,
        )?;
        if !candidates.is_empty() {
            return Ok(candidates);
        }
    }

    latest_memory_candidates(conn, &query_tokens, 200, query_embedding, half_life_days)
}

fn latest_memory_candidates(
    conn: &Connection,
    query_tokens: &[String],
    limit: u32,
    query_embedding: Option<&QueryEmbedding>,
    half_life_days: f32,
) -> KimetsuResult<Vec<Candidate>> {
    // MP-4d: exclude invalidated memories from retrieval. The row stays in
    // brain.db so `memory list` and replay can still see the history; only
    // the broker filters it out.
    //
    // v0.4.2: SELECT now also pulls the optional embedding + model id
    // so we can blend a cosine score with the lexical match.
    //
    // v0.5.1: SELECT also pulls `last_useful_at` so the broker can
    // apply the half-life decay term (memories that helped recently
    // outvote memories that haven't been confirmed useful in months).
    let mut stmt = conn.prepare_cached(
        "
        SELECT memory_id, scope, kind, text, confidence, created_at,
               use_count, usefulness_score, embedding, embedding_model,
               last_useful_at
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
            embedding,
            embedding_model,
            last_useful_at,
        ) = row?;
        let (cosine, row_vec) = compute_cosine_and_vec(
            query_embedding,
            embedding.as_deref(),
            embedding_model.as_deref(),
        );
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
            last_useful_at,
            half_life_days,
            None,
            cosine,
            row_vec,
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
    half_life_days: f32,
) -> KimetsuResult<Vec<Candidate>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT m.memory_id, m.scope, m.kind, m.text, m.confidence, m.created_at,
               m.use_count, m.usefulness_score, bm25(memories_fts) AS rank,
               m.embedding, m.embedding_model, m.last_useful_at
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
            row.get::<_, Option<String>>(11)?,
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
            last_useful_at,
        ) = row?;
        let fts_relevance = (-rank as f32).max(0.0);
        let (cosine, row_vec) = compute_cosine_and_vec(
            query_embedding,
            embedding.as_deref(),
            embedding_model.as_deref(),
        );
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
            last_useful_at,
            half_life_days,
            Some(fts_relevance),
            cosine,
            row_vec,
        ) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// v0.4.2 / D1e: cosine helper — returns both the cosine score and the
/// decoded row embedding vector for a memory row. Used by all three
/// memory-candidate retrieval paths (FTS, ANN, latest-recency) to
/// populate `Candidate.cosine` and `Candidate.embedding` for the
/// candidate-stage embedding-MMR pass.
///
/// Returns `(None, None)` when:
///   * `query_embedding` is None (NoopEmbedder / lean build)
///   * The row has no embedding bytes
///   * The row's `embedding_model` doesn't match the active query's
///     model id (cross-model mismatch — vectors are incomparable)
///
/// Cross-model rows are intentionally NOT blended: a row embedded
/// with `stub-d8` and a query embedded with `bge-small-en-v1.5`
/// produce meaningless dot products. Falling back to FTS for those
/// rows keeps hybrid retrieval safe across schema upgrades and
/// `kimetsu brain reindex` migrations (v0.4.3).
///
/// D1e: variant that returns both the cosine score and the decoded row
/// embedding vector. Used by callsites that need to store the vector
/// on the `Candidate` for the candidate-stage embedding-MMR pass.
/// When the row is cross-model or has no embedding, both fields are
/// `None` — identical semantics to [`compute_cosine`].
fn compute_cosine_and_vec(
    query_embedding: Option<&QueryEmbedding>,
    row_bytes: Option<&[u8]>,
    row_model: Option<&str>,
) -> (Option<f32>, Option<Vec<f32>>) {
    let q = match query_embedding {
        Some(q) => q,
        None => return (None, None),
    };
    let bytes = match row_bytes {
        Some(b) => b,
        None => return (None, None),
    };
    let model = match row_model {
        Some(m) => m,
        None => return (None, None),
    };
    if model != q.model_id {
        return (None, None);
    }
    let row_vec = match decode_embedding(bytes, Some(q.vector.len())) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let score = cosine_similarity(&q.vector, &row_vec);
    (Some(score), Some(row_vec))
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
    last_useful_at: Option<String>,
    half_life_days: f32,
    raw_relevance_override: Option<f32>,
    cosine_score: Option<f32>,
    // D1e: decoded embedding vector for this row (same model as the
    // active query embedder). None for cross-model rows, rows without
    // embeddings, or lean builds. Stored on Candidate for the
    // candidate-stage embedding-MMR pass.
    row_embedding: Option<Vec<f32>>,
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
    // v0.5.1: usefulness multiplier with half-life decay applied to
    // the *deviation from neutral*. A 6-month-old memory that scored
    // 1.5 (max boost) decays toward 1.0 (neutral) — NOT toward 0,
    // because losing confidence in old signal shouldn't penalize a
    // memory below a brand-new memory with zero history.
    let raw_multiplier = usefulness_multiplier(usefulness_score as f32, use_count as u32);
    let decay = usefulness_decay(last_useful_at.as_deref(), &created_at, half_life_days);
    let multiplier = 1.0 + (raw_multiplier - 1.0) * decay;
    let biased_relevance = raw_relevance * multiplier;
    Some(Candidate {
        raw_relevance: biased_relevance,
        embedding: row_embedding,
        cosine: cosine_score,
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

/// v0.5.1: half-life decay factor applied to the *deviation from
/// neutral* of [`usefulness_multiplier`]. Returns a value in `[0.0,
/// 1.0]` where 1.0 = "use full envelope" (memory was confirmed useful
/// recently) and 0.0 = "treat as neutral" (memory's confirmation is
/// ancient).
///
/// Reference timestamp:
///   * `last_useful_at` (set by the projector when a cited memory's
///     run ended in run.finished) if present
///   * fallback to `created_at` so a brand-new memory that's never
///     been cited yet decays from its birthday — same shape, but
///     starts fresh.
///
/// Math:
///   decay = exp(-ln(2) * age_days / half_life_days)
/// so at age == half_life the contribution is halved, at 2*half_life
/// it's quartered, etc.
///
/// Safety rails:
///   * `half_life_days <= 0` disables decay (returns 1.0) so an
///     operator can opt out via project.toml.
///   * Unparseable RFC3339 timestamps return 1.0 — fail-open so a
///     corrupted row doesn't get silently demoted out of retrieval.
pub(crate) fn usefulness_decay(
    last_useful_at: Option<&str>,
    created_at: &str,
    half_life_days: f32,
) -> f32 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let reference = last_useful_at.unwrap_or(created_at);
    let Ok(reference_ts) =
        OffsetDateTime::parse(reference, &time::format_description::well_known::Rfc3339)
    else {
        return 1.0;
    };
    let age = OffsetDateTime::now_utc() - reference_ts;
    let age_days = (age.whole_seconds().max(0) as f32) / 86_400.0;
    let exponent = -std::f32::consts::LN_2 * age_days / half_life_days;
    exponent.exp().clamp(0.0, 1.0)
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
            embedding: None,
            cosine: None,
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
            embedding: None,
            cosine: None,
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
            embedding: None,
            cosine: None,
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

/// v1.0.0: a memory whose cosine to the query clears this bar is kept by
/// the lexical floor even when it shares few query words — a genuine
/// semantic match shouldn't be pruned for lexical thinness. Inert on the
/// FTS-only hook path (cosine is always `None` there).
const SEMANTIC_KEEP_COSINE: f32 = 0.20;

/// v1.0.0: generic English function words carry no topical signal, so they
/// are stripped before the IDF-weighted lexical floor. Kept deliberately
/// small — only true stopwords. Content words like "repo" or "idea" are NOT
/// here; their commonness is handled by IDF, not a hand-maintained list.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "your", "with", "this", "that", "these",
    "those", "from", "into", "about", "what", "whats", "which", "who", "whom", "how", "why",
    "when", "where", "can", "could", "would", "should", "will", "shall", "does", "did", "was",
    "were", "been", "being", "have", "has", "had", "its", "it", "is", "as", "at", "by", "of", "to",
    "in", "on", "or", "an", "be", "do", "me", "my", "we", "us", "our", "im", "ive", "let", "lets",
    "please", "tell", "give", "show", "want", "need", "get", "got", "use", "using", "there",
    "their", "they", "them", "then", "than", "some", "any", "all", "more", "most", "such", "via",
    "per",
];

/// v1.0.0: tokenize a query into deduped CONTENT tokens — the same word
/// split as [`query_tokens`] but with stopwords removed and WITHOUT the
/// `CLASS_HINTS` tool-name expansions (those are a retrieval *boost*, not
/// part of the user's topical intent). Used only by the lexical floor.
fn content_tokens(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .map(str::trim)
        .filter(|part| part.len() >= 2)
        .map(str::to_ascii_lowercase)
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        // Stem AFTER the stopword check ("during" must not stem to "dur"
        // and dodge the list) so inflected variants share one IDF entry.
        .map(|t| light_stem(&t).to_string())
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// v1.0.0: discriminating weight for each content token over the
/// (non-invalidated) memory corpus, where `df` is the number of memories
/// whose text contains the token as a substring (matching
/// [`lexical_relevance`]'s substring semantics). Only tokens that actually
/// partition the corpus carry weight; the two useless extremes are zeroed:
///
///   * `df == N` — the token is in EVERY memory (the project name). `idf =
///     ln((N+1)/(N+1)) = 0` falls out of the formula naturally.
///   * `df == 0` — the token is in NO memory (an out-of-corpus word like a
///     generic English verb). It can't distinguish one memory from another,
///     so it's forced to 0. Leaving it at its (maximal) raw IDF would let a
///     single generic query word sink every candidate's coverage below the
///     floor — the on-topic memory that matches the *rare, in-corpus* word
///     would be wrongly pruned.
///
/// Everything in between gets `idf = ln((N+1)/(df+1))` — rarer ⇒ larger.
/// Best-effort: a query/count failure yields 0 for that token (fail-open).
fn corpus_token_idf(conn: &Connection, tokens: &[String]) -> KimetsuResult<HashMap<String, f32>> {
    let mut idf = HashMap::new();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if n == 0 {
        return Ok(idf);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT COUNT(*) FROM memories \
         WHERE invalidated_at IS NULL AND lower(text) LIKE ?1 ESCAPE '\\'",
    )?;
    for token in tokens {
        let pattern = format!("%{}%", escape_like(token));
        let df: i64 = stmt
            .query_row(params![pattern], |row| row.get(0))
            .unwrap_or(0);
        // df == 0 → out-of-corpus, can't discriminate → weight 0.
        let weight = if df == 0 {
            0.0
        } else {
            (((n + 1) as f32) / ((df + 1) as f32)).ln().max(0.0)
        };
        idf.insert(token.clone(), weight);
    }
    Ok(idf)
}

/// Escape SQL `LIKE` wildcards in a token so a literal `%`/`_` in a query
/// word can't widen the document-frequency match. Pairs with `ESCAPE '\'`.
fn escape_like(token: &str) -> String {
    token
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// v1.0.0: the IDF-weighted fraction of the query's discriminating power that
/// `summary` lexically covers, in `[0,1]`. Tokens present in the haystack
/// contribute their IDF weight to the numerator; all tokens contribute to the
/// denominator. A summary that matches only the query's low-IDF (common)
/// words scores near 0; one that matches the rare, topical words scores near
/// 1. Returns 0 when the total weight is ~0 (all tokens ubiquitous).
fn weighted_coverage(content: &[String], idf: &HashMap<String, f32>, summary: &str) -> f32 {
    let haystack = summary.to_ascii_lowercase();
    let mut total = 0.0f32;
    let mut hit = 0.0f32;
    for token in content {
        let weight = idf.get(token).copied().unwrap_or(0.0);
        total += weight;
        if weight > 0.0 && haystack.contains(token.as_str()) {
            hit += weight;
        }
    }
    if total <= f32::EPSILON {
        0.0
    } else {
        (hit / total).clamp(0.0, 1.0)
    }
}

/// v1.0.0: light query-side stemming — strip the common English inflection
/// suffixes so "benchmarked"/"benchmarking" reduce to "benchmark". Because
/// downstream matching is substring (`lexical_relevance`, the IDF `LIKE`
/// document-frequency count) and FTS-prefix (`fts_query` appends `*`), the
/// stem matches every variant in the corpus while the inflected form matches
/// none of them — an unstemmed "benchmarked" gets df=0, loses all IDF
/// weight, and the relevance floor goes blind on the query's one
/// discriminating word. Haystacks stay raw; only query tokens are stemmed.
/// Conservative: a suffix is stripped only when ≥4 chars remain, and only
/// one suffix is stripped.
fn light_stem(token: &str) -> &str {
    for suffix in ["ing", "ed", "es", "s"] {
        if let Some(stem) = token.strip_suffix(suffix)
            && stem.len() >= 4
        {
            return stem;
        }
    }
    token
}

fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .map(str::trim)
        .filter(|part| part.len() >= 2)
        .map(str::to_ascii_lowercase)
        .map(|t| light_stem(&t).to_string())
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

/// v0.8: does a capsule satisfy a requested (memory) kind? Repo/manifest
/// capsules match only by their literal `kind`; memory capsules
/// (`kind == "memory"`) match against the real kind embedded in their
/// `"scope:kind - text"` summary prefix.
fn capsule_matches_kind(capsule: &ContextCapsule, wanted: &str) -> bool {
    if capsule.kind == wanted {
        return true;
    }
    if capsule.kind == "memory"
        && let Some((prefix, _)) = capsule.summary.split_once(" - ")
        && let Some((_scope, mkind)) = prefix.split_once(':')
    {
        return mkind == wanted;
    }
    false
}

pub(crate) fn fts_query(query: &str) -> Option<String> {
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

/// D1e: candidate-stage MMR using embedding cosine similarity as the
/// redundancy measure, with Jaccard-of-summary-tokens as the fallback
/// when either candidate lacks an embedding vector.
///
/// Called BEFORE the candidate→capsule conversion so the `Candidate`
/// embedding fields are still accessible. Input must already be sorted
/// by descending score (the pipeline sorts before calling this).
///
/// Redundancy measure:
///   * Both candidates have embeddings of the same model → cosine(a, b).
///     cosine ∈ [-1, 1]; we use it directly as the overlap penalty.
///     Two paraphrases ("prefer rg" / "use ripgrep") will typically
///     share high cosine (≥0.85) and collapse to one slot.
///   * Either candidate lacks an embedding → Jaccard of summary-token
///     sets, scaled by 0.5 for cross-kind pairs (mirrors the existing
///     capsule-stage logic).
///
/// Cross-kind pairs are penalized at half the same-kind rate for both
/// measures (consistent with the capsule-stage Jaccard MMR).
fn apply_candidate_mmr_diversity(mut sorted: Vec<Candidate>, lambda: f32) -> Vec<Candidate> {
    if sorted.len() <= 1 {
        return sorted;
    }
    // Pre-tokenize summaries for the Jaccard fallback.
    let summaries: Vec<std::collections::HashSet<String>> = sorted
        .iter()
        .map(|c| summary_token_set(&c.capsule.summary))
        .collect();

    let mut picked_indices: Vec<usize> = Vec::with_capacity(sorted.len());
    let mut remaining: Vec<usize> = (0..sorted.len()).collect();

    // Seed with the highest-scoring candidate.
    picked_indices.push(remaining.remove(0));

    while !remaining.is_empty() {
        let mut best_idx_in_remaining = 0;
        let mut best_score = f32::MIN;

        for (i, &cand) in remaining.iter().enumerate() {
            let mut max_overlap = 0.0f32;
            for &p in &picked_indices {
                // Compute redundancy between candidate `cand` and
                // already-picked `p`.
                let same_kind = sorted[cand].capsule.kind == sorted[p].capsule.kind;
                let raw_overlap = candidate_pair_overlap(
                    &sorted[cand],
                    &sorted[p],
                    &summaries[cand],
                    &summaries[p],
                );
                let overlap = if same_kind {
                    raw_overlap
                } else {
                    raw_overlap * 0.5
                };
                if overlap > max_overlap {
                    max_overlap = overlap;
                }
            }
            let mmr = lambda * sorted[cand].capsule.score - (1.0 - lambda) * max_overlap;
            if mmr > best_score {
                best_score = mmr;
                best_idx_in_remaining = i;
            }
        }
        picked_indices.push(remaining.remove(best_idx_in_remaining));
    }

    // Reconstruct in picked order.
    let mut taken: Vec<Option<Candidate>> = sorted.drain(..).map(Some).collect();
    let mut out = Vec::with_capacity(taken.len());
    for idx in picked_indices {
        if let Some(c) = taken[idx].take() {
            out.push(c);
        }
    }
    out
}

/// D1e: overlap between two candidates for MMR.
///
/// * Both have embeddings → cosine similarity (clamped to [0,1] to
///   treat anti-correlated vectors as non-redundant, not negatively
///   redundant).
/// * Either lacks an embedding → Jaccard of summary-token sets.
fn candidate_pair_overlap(
    a: &Candidate,
    b: &Candidate,
    tokens_a: &std::collections::HashSet<String>,
    tokens_b: &std::collections::HashSet<String>,
) -> f32 {
    if let (Some(va), Some(vb)) = (a.embedding.as_deref(), b.embedding.as_deref()) {
        // Cosine in [-1,1]; clamp to [0,1] so negative correlation
        // (very different content) contributes 0 overlap rather than
        // a negative penalty (which would spuriously boost unrelated
        // content over moderately-related content).
        cosine_similarity(va, vb).max(0.0)
    } else {
        jaccard(tokens_a, tokens_b)
    }
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

// -----------------------------------------------------------------------
// F2: capsule resolver — expand a headline handle to its full text.
// -----------------------------------------------------------------------

/// Maximum bytes returned when resolving a `file:` handle. Keeps large
/// source files from flooding the context window on a single expand call.
const FILE_EXPAND_CAP_BYTES: usize = 2048;

/// F2: resolve an expansion handle to its full text content.
///
/// Handles:
/// - `memory:<id>` → `SELECT text FROM memories WHERE memory_id = ?`
/// - `file:<path>` → read `repo_root/<path>`, capped at [`FILE_EXPAND_CAP_BYTES`]
/// - `run:<id>`    → deferred; returns a descriptive error
/// - anything else → returns a descriptive error
///
/// This is the resolver that the `expand_capsule` agent tool delegates to.
/// All errors are user-visible (returned to the agent as a tool-result
/// error string) and never crash the dispatch loop.
pub fn resolve_capsule(
    conn: &Connection,
    repo_root: &std::path::Path,
    handle: &str,
) -> kimetsu_core::KimetsuResult<String> {
    if let Some(memory_id) = handle.strip_prefix("memory:") {
        // SELECT the raw text from the memories table.
        let mut stmt = conn.prepare_cached(
            "SELECT text FROM memories WHERE memory_id = ? AND invalidated_at IS NULL",
        )?;
        let text: Option<String> = stmt
            .query_row(rusqlite::params![memory_id], |row| row.get(0))
            .optional()?;
        match text {
            Some(t) => Ok(t),
            None => {
                Err(format!("expand_capsule: no active memory found for handle `{handle}`").into())
            }
        }
    } else if let Some(rel_path) = handle.strip_prefix("file:") {
        // Sanitize: reject absolute paths (drive-letter or Unix-root) and
        // `..` traversal. On Windows, POSIX-style `/foo` paths are not
        // considered absolute by `is_absolute()` (no drive prefix), so we
        // also reject paths with a RootDir component.
        let path = std::path::Path::new(rel_path);
        if path.is_absolute() {
            return Err(format!(
                "expand_capsule: `{handle}` is an absolute path — only repo-relative paths are supported"
            )
            .into());
        }
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    return Err(format!(
                        "expand_capsule: `{handle}` contains `..` traversal — rejected"
                    )
                    .into());
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    return Err(format!(
                        "expand_capsule: `{handle}` is an absolute path — only repo-relative paths are supported"
                    )
                    .into());
                }
                _ => {}
            }
        }
        let full_path = repo_root.join(path);
        let bytes = std::fs::read(&full_path)
            .map_err(|e| format!("expand_capsule: could not read `{rel_path}`: {e}"))?;
        // Bound the returned slice so huge files don't blow the context window.
        let bounded = if bytes.len() > FILE_EXPAND_CAP_BYTES {
            let mut end = FILE_EXPAND_CAP_BYTES;
            // Snap back to a UTF-8 boundary so we don't slice mid-codepoint.
            while end > 0 && (bytes[end] & 0xC0) == 0x80 {
                end -= 1;
            }
            let s = String::from_utf8_lossy(&bytes[..end]);
            format!(
                "{s}\n[... truncated at {FILE_EXPAND_CAP_BYTES} bytes; call expand_capsule again with a line range if needed]"
            )
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok(bounded)
    } else if handle.starts_with("run:") {
        Err(format!(
            "expand_capsule: `run:` handle expansion is not yet supported (handle: `{handle}`)"
        )
        .into())
    } else {
        Err(format!(
            "expand_capsule: unrecognised handle format `{handle}`; \
             expected `memory:<id>`, `file:<path>`, or `run:<id>`"
        )
        .into())
    }
}

// ── v1.0.0: cross-encoder reranking ──────────────────────────────────────

/// v1.0.0: final-stage cross-encoder rerank over already-retrieved capsules.
/// Reranks by `summary`, overwrites `score` with the sigmoid-normalized
/// rerank score, sorts descending, drops capsules below `floor`, truncates
/// to `cap` (0 = no cap). Fail-open: on a rerank error the input ordering
/// is returned unchanged (truncated to `cap`) — a broken reranker must
/// never lose retrieval entirely.
pub fn rerank_capsules(
    query: &str,
    capsules: Vec<ContextCapsule>,
    reranker: &dyn crate::embeddings::Reranker,
    floor: f32,
    cap: usize,
) -> Vec<ContextCapsule> {
    if capsules.is_empty() {
        return capsules;
    }

    // Rerank on the FULL summary. Truncating to a snippet was tried for
    // latency and measurably cratered quality on the eval fixture
    // (recall@4 0.83 → 0.66, below even FTS) — the cross-encoder needs the
    // whole lesson to judge relevance. Reranking is therefore a
    // quality-over-latency opt-in, not part of the hook's 300ms budget.
    let docs: Vec<&str> = capsules.iter().map(|c| c.summary.as_str()).collect();
    let scores = match reranker.rerank(query, &docs) {
        // The trait contract is one score per doc in doc order; a custom
        // third-party reranker that returns a short vec would otherwise
        // silently drop the unscored tail via the zip below — treat a
        // length mismatch as an error and fail open instead.
        Ok(s) if s.len() == docs.len() => s,
        _ => {
            // Fail-open: preserve input order, just apply cap.
            let mut out = capsules;
            if cap > 0 && out.len() > cap {
                out.truncate(cap);
            }
            return out;
        }
    };

    let mut ranked: Vec<ContextCapsule> = capsules
        .into_iter()
        .zip(scores)
        .map(|(mut c, s)| {
            c.score = s;
            c
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    ranked.retain(|c| c.score >= floor);

    if cap > 0 && ranked.len() > cap {
        ranked.truncate(cap);
    }

    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(kind: &str, summary: &str) -> ContextCapsule {
        ContextCapsule {
            id: "c".into(),
            kind: kind.into(),
            summary: summary.into(),
            token_estimate: 1,
            expansion_handle: "memory:x".into(),
            provenance: vec![],
            confidence: 1.0,
            freshness: 1.0,
            relevance: 1.0,
            scope_weight: 1.0,
            score: 1.0,
        }
    }

    /// Create a unique temp directory under the system temp path.
    /// Named by `tag` so test failures are diagnosable.
    fn make_test_dir(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("kbrain_test_{tag}_{ts}"));
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn capsule_matches_kind_reads_memory_summary_prefix() {
        // Memory capsule: real kind lives in the "scope:kind - text" prefix.
        let mem = capsule("memory", "project:failure_pattern - linker not found");
        assert!(capsule_matches_kind(&mem, "failure_pattern"));
        assert!(!capsule_matches_kind(&mem, "command"));
        // Non-memory capsules match only by literal kind, never via prefix.
        let repo = capsule("repo_file", "src/lib.rs:command - run build");
        assert!(capsule_matches_kind(&repo, "repo_file"));
        assert!(!capsule_matches_kind(&repo, "command"));
    }

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
                ..Default::default()
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
                ..Default::default()
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

    // ----- v0.5.1: usefulness decay -----

    /// v0.5.1: `half_life_days <= 0` is the operator opt-out hatch.
    /// Decay must short-circuit to 1.0 so the usefulness multiplier
    /// is unmodified — exact pre-v0.5.1 behavior for projects that
    /// set `decay_half_life_days = 0` in project.toml.
    #[test]
    fn usefulness_decay_disabled_when_half_life_is_zero_or_negative() {
        // Even a 5-year-old reference returns 1.0 with decay disabled.
        let ancient = "2021-01-01T00:00:00Z";
        assert!((usefulness_decay(Some(ancient), ancient, 0.0) - 1.0).abs() < f32::EPSILON);
        assert!((usefulness_decay(Some(ancient), ancient, -1.0) - 1.0).abs() < f32::EPSILON);
    }

    /// v0.5.1: unparseable timestamps return 1.0 (fail-open). A
    /// corrupted row shouldn't get silently dropped out of retrieval
    /// just because its `last_useful_at` got mangled.
    #[test]
    fn usefulness_decay_returns_one_on_unparseable_timestamps() {
        assert!(
            (usefulness_decay(Some("not-a-date"), "also-not", 30.0) - 1.0).abs() < f32::EPSILON
        );
    }

    /// v0.5.1: a memory whose reference timestamp is "now" (no age)
    /// decays by zero — full contribution.
    #[test]
    fn usefulness_decay_full_at_zero_age() {
        // Use a timestamp from the future so age clamps to 0.
        let future = "2099-01-01T00:00:00Z";
        let d = usefulness_decay(Some(future), future, 30.0);
        assert!((d - 1.0).abs() < f32::EPSILON, "got {d}");
    }

    /// v0.5.1: at age == half_life, decay = 0.5; at age = 2 * half_life,
    /// decay = 0.25. Computed by setting `last_useful_at` to (now - days)
    /// using OffsetDateTime arithmetic — the only way to get a stable
    /// "now-relative" timestamp without freezing the clock.
    #[test]
    fn usefulness_decay_follows_half_life_curve() {
        let half_life = 10.0_f32;
        let now = OffsetDateTime::now_utc();
        let fmt = &time::format_description::well_known::Rfc3339;

        // age = half_life -> decay ~= 0.5
        let one_half_life_ago = (now - time::Duration::seconds((half_life * 86_400.0) as i64))
            .format(fmt)
            .expect("format");
        let d1 = usefulness_decay(Some(&one_half_life_ago), &one_half_life_ago, half_life);
        assert!(
            (d1 - 0.5).abs() < 0.01,
            "expected ~0.5 at one half-life, got {d1}"
        );

        // age = 2 * half_life -> decay ~= 0.25
        let two_half_lives_ago = (now
            - time::Duration::seconds((2.0 * half_life * 86_400.0) as i64))
        .format(fmt)
        .expect("format");
        let d2 = usefulness_decay(Some(&two_half_lives_ago), &two_half_lives_ago, half_life);
        assert!(
            (d2 - 0.25).abs() < 0.01,
            "expected ~0.25 at two half-lives, got {d2}"
        );
    }

    /// v0.5.1: when `last_useful_at` is None the function falls back to
    /// `created_at`. A 1-day-old never-cited memory should still get
    /// nearly-full decay (close to 1.0) for a 30-day half-life.
    #[test]
    fn usefulness_decay_falls_back_to_created_at_when_last_useful_is_none() {
        let now = OffsetDateTime::now_utc();
        let fmt = &time::format_description::well_known::Rfc3339;
        let one_day_ago = (now - time::Duration::seconds(86_400))
            .format(fmt)
            .expect("format");
        let d = usefulness_decay(None, &one_day_ago, 30.0);
        // exp(-ln(2) / 30) ≈ 0.977
        assert!(
            (d - 0.977).abs() < 0.01,
            "expected ~0.977 for 1-day-old created_at under 30d half-life, got {d}"
        );
    }

    /// v0.5.1: end-to-end retrieval test. Two memories with identical
    /// lexical match, identical use_count, identical (max) usefulness
    /// score — one cited yesterday, one cited a year ago. Decay must
    /// rank the recent one first.
    #[test]
    fn aged_cited_memory_ranks_below_recently_cited_memory() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        let now = OffsetDateTime::now_utc();
        let fmt = &time::format_description::well_known::Rfc3339;
        let one_day_ago = (now - time::Duration::seconds(86_400))
            .format(fmt)
            .expect("format");
        let one_year_ago = (now - time::Duration::seconds(365 * 86_400))
            .format(fmt)
            .expect("format");

        // Both memories say "use ripgrep for code search", both have
        // use_count = 5, usefulness_score = 5 (max boost → 1.5
        // multiplier). The only difference is `last_useful_at`.
        for (mid, last_useful) in [("m_recent", &one_day_ago), ("m_aged", &one_year_ago)] {
            let text = "use ripgrep for code search";
            let normalized = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score, last_useful_at
                )
                VALUES (?1, 'global_user', 'fact', ?2, ?3, 1.0, NULL, '{}',
                        '2024-01-01T00:00:00Z', 5, 5.0, ?4)
                ",
                rusqlite::params![mid, text, normalized, last_useful],
            )
            .expect("insert memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'global_user')",
                rusqlite::params![mid, text],
            )
            .expect("insert fts");
        }

        // Default broker weights → 30-day half-life. 1 year ≈ 12 half-lives.
        let weights = kimetsu_core::config::BrokerWeights::default();
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "ripgrep search".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve");

        let mem_order: Vec<&str> = bundle
            .capsules
            .iter()
            .filter_map(|c| c.expansion_handle.strip_prefix("memory:"))
            .collect();
        assert_eq!(
            mem_order.first().copied(),
            Some("m_recent"),
            "recently-cited memory must rank first under decay; got order {mem_order:?}"
        );
    }

    /// v0.5.1: with decay disabled (half_life = 0) the aged + recent
    /// memories tie and the deterministic tiebreaker (id) decides —
    /// proves the ranking flip in the previous test is *caused* by
    /// decay, not by some unrelated side effect of the timestamp.
    #[test]
    fn aged_cited_memory_does_not_decay_when_half_life_is_zero() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        let now = OffsetDateTime::now_utc();
        let fmt = &time::format_description::well_known::Rfc3339;
        let one_day_ago = (now - time::Duration::seconds(86_400))
            .format(fmt)
            .expect("format");
        let one_year_ago = (now - time::Duration::seconds(365 * 86_400))
            .format(fmt)
            .expect("format");

        for (mid, last_useful) in [("m_recent", &one_day_ago), ("m_aged", &one_year_ago)] {
            let text = "use ripgrep for code search";
            let normalized = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score, last_useful_at
                )
                VALUES (?1, 'global_user', 'fact', ?2, ?3, 1.0, NULL, '{}',
                        '2024-01-01T00:00:00Z', 5, 5.0, ?4)
                ",
                rusqlite::params![mid, text, normalized, last_useful],
            )
            .expect("insert memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'global_user')",
                rusqlite::params![mid, text],
            )
            .expect("insert fts");
        }

        // Disable decay via broker config.
        let weights = kimetsu_core::config::BrokerWeights {
            decay_half_life_days: 0.0,
            ..Default::default()
        };

        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "ripgrep search".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve");

        // Both memories should surface. With decay disabled, their
        // scores are identical (same multiplier, same lexical match,
        // same freshness band since both created_at are equal). The
        // sort tiebreaker falls back to id, so m_aged < m_recent
        // alphabetically.
        let scores: Vec<(String, f32)> = bundle
            .capsules
            .iter()
            .filter_map(|c| {
                c.expansion_handle
                    .strip_prefix("memory:")
                    .map(|id| (id.to_string(), c.score))
            })
            .collect();
        assert_eq!(scores.len(), 2, "both memories should surface");
        let recent_score = scores
            .iter()
            .find(|(id, _)| id == "m_recent")
            .map(|(_, s)| *s)
            .expect("m_recent present");
        let aged_score = scores
            .iter()
            .find(|(id, _)| id == "m_aged")
            .map(|(_, s)| *s)
            .expect("m_aged present");
        // With decay off, the two multipliers are equal → scores match.
        assert!(
            (recent_score - aged_score).abs() < 1e-4,
            "with decay disabled the two memories should tie on score: recent={recent_score} aged={aged_score}"
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
                ..Default::default()
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

    // ---------------------------------------------------------------
    // D1d tests: ANN index correctness, rebuild on model change, dedup
    // ---------------------------------------------------------------

    /// D1d test 1: ANN finds a semantic match that FTS misses.
    ///
    /// Strategy: use a manually-crafted ("oracle") embedder that returns a
    /// FIXED known vector for any input, paired with a direct-SQL memory
    /// insertion that stores the SAME vector for the "semantic" memory and a
    /// DIFFERENT vector for the "lexical decoy". The query text and memory
    /// texts deliberately share NO words, so FTS returns nothing. ANN
    /// surfaces the semantically-near memory via the usearch index.
    ///
    /// Concretely:
    ///   - query text = "phosphorescent bioluminescent organism" (no overlap
    ///     with any memory text)
    ///   - m_semantic text = "cookie recipe chocolate" — completely different
    ///     words, but we MANUALLY store the same vector as the query embedding.
    ///   - m_decoy text = "git rebase squash commits" — different text,
    ///     orthogonal vector.
    ///
    /// The "oracle" embedder always returns [1,0,0,0,0,0,0,0] for any text.
    /// We store [1,0,0,0,0,0,0,0] for m_semantic and [0,1,0,0,0,0,0,0] for
    /// m_decoy. Cosine("oracle query", m_semantic) = 1.0; cosine(query,
    /// m_decoy) = 0.0. FTS finds nothing (no shared tokens). ANN finds
    /// m_semantic as the nearest neighbour.
    #[cfg(feature = "embeddings")]
    #[test]
    fn ann_finds_semantic_match_fts_misses() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        // Oracle embedder: always returns the same unit vector regardless of text.
        // This lets us control cosine similarity independently of word overlap.
        struct OracleEmbedder;
        impl embeddings::Embedder for OracleEmbedder {
            fn embed(&self, _text: &str) -> Result<Vec<f32>, embeddings::EmbedderError> {
                // [1,0,0,0,0,0,0,0] — unit vector along dim-0
                Ok(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            }
            fn model_id(&self) -> &str {
                "oracle-d8"
            }
            fn dim(&self) -> usize {
                8
            }
        }

        let model_id = "oracle-d8";

        // m_semantic: text shares NO tokens with the query, but stored
        // embedding is [1,0,...,0] — cosine with the oracle query vector = 1.0.
        let sem_vec = embeddings::encode_embedding(&[1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let sem_text = "cookie recipe chocolate";
        let sem_norm = kimetsu_core::memory::normalize_memory_text(sem_text);
        conn.execute(
            "INSERT INTO memories (
                 memory_id, scope, kind, text, normalized_text, confidence,
                 source_event_id, provenance_snapshot_json, created_at,
                 use_count, usefulness_score, embedding, embedding_model
             )
             VALUES ('m_semantic', 'global_user', 'fact', ?1, ?2, 1.0, NULL, '{}',
                     '2026-01-01T00:00:00Z', 0, 0.0, ?3, ?4)",
            rusqlite::params![sem_text, sem_norm, sem_vec, model_id],
        )
        .expect("insert m_semantic");
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope)
             VALUES ('m_semantic', ?1, 'fact', 'global_user')",
            rusqlite::params![sem_text],
        )
        .expect("insert m_semantic fts");

        // m_decoy: different text, orthogonal vector [0,1,0,...,0].
        let decoy_vec = embeddings::encode_embedding(&[0.0f32, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let decoy_text = "git rebase squash commits";
        let decoy_norm = kimetsu_core::memory::normalize_memory_text(decoy_text);
        conn.execute(
            "INSERT INTO memories (
                 memory_id, scope, kind, text, normalized_text, confidence,
                 source_event_id, provenance_snapshot_json, created_at,
                 use_count, usefulness_score, embedding, embedding_model
             )
             VALUES ('m_decoy', 'global_user', 'fact', ?1, ?2, 1.0, NULL, '{}',
                     '2026-01-01T00:00:00Z', 0, 0.0, ?3, ?4)",
            rusqlite::params![decoy_text, decoy_norm, decoy_vec, model_id],
        )
        .expect("insert m_decoy");
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope)
             VALUES ('m_decoy', ?1, 'fact', 'global_user')",
            rusqlite::params![decoy_text],
        )
        .expect("insert m_decoy fts");

        // Sanity: FTS must find nothing for the query tokens.
        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts \
                 WHERE memories_fts MATCH 'phosphorescent bioluminescent'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            fts_hits, 0,
            "sanity: query tokens must not appear in any memory text"
        );

        // Retrieve via oracle embedder.
        // query = "phosphorescent bioluminescent organism" has no lexical
        // overlap with either memory. ANN must surface m_semantic (cosine=1).
        let weights = kimetsu_core::config::BrokerWeights::default();
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "phosphorescent bioluminescent organism".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
            &[],
            &OracleEmbedder,
        )
        .expect("retrieve");

        let handles: Vec<&str> = bundle
            .capsules
            .iter()
            .filter_map(|c| c.expansion_handle.strip_prefix("memory:"))
            .collect();

        assert!(
            handles.contains(&"m_semantic"),
            "ANN must surface m_semantic (cosine=1 with oracle query) even though \
             FTS found nothing; got handles: {handles:?}"
        );
    }

    /// D1d test 3: a memory matched by both FTS and ANN appears exactly once.
    #[cfg(feature = "embeddings")]
    #[test]
    fn dedup_memory_matched_by_fts_and_ann_appears_once() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        let stub = embeddings::StubEmbedder::new();

        // This memory contains "ripgrep" (lexical) AND has a stub embedding
        // derived from its text, so the query "ripgrep" matches it both via
        // FTS and via ANN (same words → same stub bucket vector).
        insert_memory_with_embedding(&conn, "m_both", "use ripgrep for fast search", &stub);

        let weights = kimetsu_core::config::BrokerWeights::default();
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "ripgrep".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
            &[],
            &stub,
        )
        .expect("retrieve");

        let count = bundle
            .capsules
            .iter()
            .filter(|c| c.expansion_handle == "memory:m_both")
            .count();
        assert_eq!(
            count,
            1,
            "m_both (matched by both FTS and ANN) must appear exactly once; \
             bundle: {:?}",
            bundle
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );
    }

    // ---------------------------------------------------------------
    // D1e tests: embedding-MMR deduplication + semantic relevance floor
    // ---------------------------------------------------------------

    /// D1e-a (embeddings-gated): two paraphrased memories that share an
    /// almost-identical embedding vector (cosine = 1.0, so embedding-MMR
    /// sees them as maximally redundant) but have LOW Jaccard overlap on
    /// their summary tokens (different words, so the Jaccard-only capsule-
    /// stage MMR would NOT penalize the second one and both survive the
    /// budget with max_capsules=2).
    ///
    /// Key mechanic: embedding-MMR assigns the second near-duplicate a very
    /// negative MMR score (lambda * score - (1-lambda) * 1.0 < 0 when score
    /// is small). It therefore ends up LAST in the reordered candidate list.
    /// When max_capsules=1 it is excluded. With Jaccard-only (NoopEmbedder),
    /// the second paraphrase has low Jaccard overlap → survives when
    /// max_capsules=2.
    ///
    /// Expected result:
    ///   * OracleEmbedder + max_capsules=1: ONE paraphrase (embedding-MMR
    ///     collapsed the redundant one).
    ///   * NoopEmbedder + max_capsules=2: BOTH paraphrases survive (Jaccard
    ///     does not see them as redundant — different tokens).
    #[cfg(feature = "embeddings")]
    #[test]
    fn embedding_mmr_collapses_paraphrases_but_jaccard_does_not() {
        // OracleEmbedder: always returns [1,0,0,…] (dim=8).
        // cosine(any two texts) = 1.0 → maximal redundancy in embedding space.
        struct OracleEmbedder;
        impl embeddings::Embedder for OracleEmbedder {
            fn embed(&self, _text: &str) -> Result<Vec<f32>, embeddings::EmbedderError> {
                let mut v = vec![0.0f32; 8];
                v[0] = 1.0;
                Ok(v)
            }
            fn model_id(&self) -> &str {
                "oracle-d8"
            }
            fn dim(&self) -> usize {
                8
            }
        }

        // Setup: two memories with DIFFERENT words (low Jaccard) but
        // SAME oracle embedding (cosine = 1.0).
        let oracle = OracleEmbedder;
        let weights = kimetsu_core::config::BrokerWeights::default();

        // "prefer ripgrep" vs "rg is the fastest" — entirely different tokens.
        // Summary token-set overlap ≈ 0 ⟹ Jaccard ≈ 0.
        let m_rg1_text = "prefer ripgrep for searching source code";
        let m_rg2_text = "rg is the fastest way to locate patterns";

        // --- Embedding-MMR path (OracleEmbedder), max_capsules=1 ---
        // Under embedding-MMR: second paraphrase gets MMR score
        //   0.7 * score - 0.3 * 1.0  (overlap = cosine = 1.0)
        // For any small normalised score, this is negative → it is assigned
        // last in the MMR reordering. max_capsules=1 → only 1 included.
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        insert_memory_with_embedding(&conn, "m_rg1", m_rg1_text, &oracle);
        insert_memory_with_embedding(&conn, "m_rg2", m_rg2_text, &oracle);

        let bundle_embedding = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                // Query that matches both via FTS so they survive pre-MMR scoring.
                query: "search source patterns".to_string(),
                budget_tokens: 20_000,
                max_capsules: 1, // tight cap: only 1 slot available
                ..Default::default()
            },
            &[],
            &oracle,
        )
        .expect("retrieve with oracle embedder");

        // Under embedding-MMR, the second paraphrase (cosine=1.0 with first)
        // is reranked last and excluded by max_capsules=1.
        let emb_in_capsules = bundle_embedding
            .capsules
            .iter()
            .filter(|c| {
                c.expansion_handle == "memory:m_rg1" || c.expansion_handle == "memory:m_rg2"
            })
            .count();
        assert_eq!(
            emb_in_capsules,
            1,
            "embedding-MMR must collapse cosine=1.0 paraphrases: with max_capsules=1 \
             only ONE should be included; capsule handles: {:?}; excluded: {:?}",
            bundle_embedding
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>(),
            bundle_embedding
                .excluded
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );

        // At least one is in excluded (the redundant near-duplicate).
        let emb_in_excluded = bundle_embedding
            .excluded
            .iter()
            .filter(|c| {
                c.expansion_handle == "memory:m_rg1" || c.expansion_handle == "memory:m_rg2"
            })
            .count();
        assert_eq!(
            emb_in_excluded,
            1,
            "the second near-duplicate must be in excluded under embedding-MMR; \
             excluded handles: {:?}",
            bundle_embedding
                .excluded
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );

        // --- Lean/Jaccard-only path (NoopEmbedder), max_capsules=2 ---
        // With Jaccard-only: summary tokens of m_rg1 and m_rg2 have ≈0
        // overlap (different words) → low redundancy penalty → BOTH score
        // high under MMR → both survive with max_capsules=2.
        let conn2 = rusqlite::Connection::open_in_memory().expect("in-memory 2");
        crate::schema::initialize(&conn2).expect("init schema 2");
        insert_memory_with_embedding(&conn2, "m_rg1", m_rg1_text, &oracle);
        insert_memory_with_embedding(&conn2, "m_rg2", m_rg2_text, &oracle);

        let bundle_lean = retrieve_context_with_embedder(
            &conn2,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "search source patterns".to_string(),
                budget_tokens: 20_000,
                max_capsules: 2, // room for both
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve with NoopEmbedder");

        let lean_in_capsules = bundle_lean
            .capsules
            .iter()
            .filter(|c| {
                c.expansion_handle == "memory:m_rg1" || c.expansion_handle == "memory:m_rg2"
            })
            .count();
        assert_eq!(
            lean_in_capsules,
            2,
            "Jaccard-only path must NOT collapse the two paraphrases (different words, \
             low token overlap → both survive MMR with max_capsules=2); capsule handles: {:?}",
            bundle_lean
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );
    }

    // ── v1.0.0: lexical relevance floor (A+B+C) ──────────────────────────

    #[test]
    fn content_tokens_strips_stopwords_keeps_topical_words() {
        let got = content_tokens("Tell me about kimetsu, what's the idea of the repo");
        // Stopwords (tell, me, about, what, the, of) dropped; "s" too short.
        // Topical words kept; deduped (no second "the").
        assert_eq!(got, vec!["kimetsu", "idea", "repo"]);
    }

    #[test]
    fn light_stem_strips_one_inflection_suffix() {
        assert_eq!(light_stem("benchmarked"), "benchmark");
        assert_eq!(light_stem("benchmarking"), "benchmark");
        assert_eq!(light_stem("repos"), "repo");
        // Too short after stripping → untouched.
        assert_eq!(light_stem("does"), "does");
        assert_eq!(light_stem("toml"), "toml");
    }

    /// Real-world regression: "Can you find out how kimetsu is benchmarked?"
    /// surfaced off-topic memories because the inflected "benchmarked"
    /// matched nothing (FTS prefix `benchmarked*` and IDF `%benchmarked%`
    /// both miss "benchmark"), zeroing the query's only discriminating
    /// token. With query-side stemming the benchmark memory surfaces and
    /// the off-topic ones stay below the floor.
    #[test]
    fn stemmed_query_matches_inflected_corpus_through_floor() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        let insert = |id: &str, text: &str| {
            let norm = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score, embedding, embedding_model
                 )
                 VALUES (?1, 'global_user', 'fact', ?2, ?3, 0.9, NULL, '{}',
                         '2026-06-01T00:00:00Z', 0, 0.0, NULL, NULL)",
                rusqlite::params![id, text, norm],
            )
            .expect("insert memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'global_user')",
                rusqlite::params![id, text],
            )
            .expect("insert fts");
        };
        insert(
            "m_bench",
            "kimetsu benchmark runs go through the kbench binary and the Terminal-Bench driver",
        );
        insert(
            "m_doctor",
            "kimetsu doctor version-skew check parses process start times on Windows via CIM",
        );
        insert(
            "m_gc",
            "kimetsu runs auto-GC on run creation; keep the env guard at the trigger site",
        );

        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &kimetsu_core::config::BrokerWeights::default(),
            ContextRequest {
                stage: "localization".to_string(),
                query: "Can you find out how kimetsu is benchmarked?".to_string(),
                budget_tokens: 2000,
                max_capsules: 2,
                min_lexical_coverage: 0.5,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve");
        let handles: Vec<_> = bundle
            .capsules
            .iter()
            .map(|c| c.expansion_handle.as_str())
            .collect();
        assert!(
            handles.contains(&"memory:m_bench"),
            "stemmed 'benchmarked' must surface the benchmark memory; got {handles:?}"
        );
        assert!(
            !handles.contains(&"memory:m_doctor") && !handles.contains(&"memory:m_gc"),
            "off-topic memories sharing only 'kimetsu' must stay below the floor; got {handles:?}"
        );
    }

    #[test]
    fn weighted_coverage_ignores_zero_idf_tokens() {
        // "kimetsu" is corpus-ubiquitous (idf 0); "idea" is rare (high idf);
        // "repo" is mid. A summary that matches only the project name + a
        // mid-idf word covers a minority of the discriminating weight.
        let content = vec![
            "kimetsu".to_string(),
            "idea".to_string(),
            "repo".to_string(),
        ];
        let mut idf = HashMap::new();
        idf.insert("kimetsu".to_string(), 0.0);
        idf.insert("idea".to_string(), 1.386);
        idf.insert("repo".to_string(), 0.693);

        // Matches kimetsu + repo, NOT idea → 0.693 / (1.386+0.693) ≈ 0.333.
        let cov = weighted_coverage(
            &content,
            &idf,
            "global:fact - the git repo and kimetsu brain",
        );
        assert!((cov - 0.333).abs() < 0.01, "got {cov}");

        // Matches the rare topical word → high coverage.
        let cov_topical =
            weighted_coverage(&content, &idf, "global:fact - the core idea of kimetsu");
        assert!(cov_topical > 0.6, "got {cov_topical}");
    }

    #[test]
    fn escape_like_neutralizes_wildcards() {
        assert_eq!(escape_like("a_b%c"), "a\\_b\\%c");
        assert_eq!(escape_like("plain"), "plain");
    }

    /// The reported regression, reproduced end-to-end on the FTS-only path:
    /// a corpus of unrelated debugging war-stories that all happen to contain
    /// the project name "kimetsu", queried with a broad conceptual prompt.
    ///
    /// * floor disabled (min_lexical_coverage = 0.0) → all the noise surfaces
    ///   (pre-fix behaviour: incidental "kimetsu" overlap is enough).
    /// * floor enabled (0.5) → the memories whose ONLY match is the corpus-
    ///   ubiquitous project name (m2, m3) are dropped. m1 also contains the
    ///   real word "repo", so it's a genuine (if weak) lexical match and
    ///   survives — eliminating that kind of keyword-overlap-but-off-topic
    ///   hit needs the semantic path, not lexical filtering. The win here is
    ///   killing the pure-project-name matches, which were the bulk of the
    ///   injected noise.
    #[test]
    fn lexical_floor_drops_offtopic_memories_sharing_project_name() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        let insert = |id: &str, text: &str| {
            let norm = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score, embedding, embedding_model
                 )
                 VALUES (?1, 'global_user', 'fact', ?2, ?3, 0.9, NULL, '{}',
                         '2026-06-01T00:00:00Z', 0, 0.0, NULL, NULL)",
                rusqlite::params![id, text, norm],
            )
            .expect("insert memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'global_user')",
                rusqlite::params![id, text],
            )
            .expect("insert fts");
        };

        // All three contain "kimetsu"; none contain "idea". Only m1 contains
        // "repo" (as in "git repo") — mirrors the real war-stories.
        insert(
            "m1",
            "When implementing a setup command that calls init_project, tests must call \
             git_init_boundary before setup_cmd so ProjectPaths discover resolves to the temp \
             dir instead of climbing to the real parent git repo including the user brain at kimetsu",
        );
        insert(
            "m2",
            "A member crate with default embeddings silently turned embeddings on for the entire \
             cargo test workspace build graph because cargo unifies features; kimetsu-chat \
             retrieval tests failed",
        );
        insert(
            "m3",
            "In toml 0.9 use toml from_str to parse a TOML document into a Value not str parse; \
             implementing config get and set in kimetsu-cli",
        );

        let query = "Tell me about kimetsu, what's the idea of the repo".to_string();
        let weights = kimetsu_core::config::BrokerWeights::default();
        let handles = |bundle: &ContextBundle| {
            bundle
                .capsules
                .iter()
                .map(|c| c.expansion_handle.clone())
                .collect::<Vec<_>>()
        };

        // Floor disabled: every off-topic memory surfaces (pre-fix behaviour).
        let no_floor = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: query.clone(),
                budget_tokens: 2000,
                max_capsules: 8,
                min_lexical_coverage: 0.0,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve without floor");
        let before = handles(&no_floor);
        assert!(
            before.contains(&"memory:m2".to_string()) && before.contains(&"memory:m3".to_string()),
            "sanity: without the floor the pure-project-name memories should surface; got {before:?}"
        );

        // Floor enabled: the pure-project-name matches (m2, m3) are dropped.
        let floored = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query,
                budget_tokens: 2000,
                max_capsules: 8,
                min_lexical_coverage: 0.5,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve with floor");
        let after = handles(&floored);
        assert!(
            !after.contains(&"memory:m2".to_string()) && !after.contains(&"memory:m3".to_string()),
            "the lexical floor must drop memories whose only match is the corpus-ubiquitous \
             project name; surviving: {after:?}"
        );
    }

    /// A genuinely on-topic query must NOT be over-pruned: a memory that
    /// covers the query's rare, discriminating word survives the floor.
    #[test]
    fn lexical_floor_keeps_ontopic_memory() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        let insert = |id: &str, text: &str| {
            let norm = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score, embedding, embedding_model
                 )
                 VALUES (?1, 'global_user', 'fact', ?2, ?3, 0.9, NULL, '{}',
                         '2026-06-01T00:00:00Z', 0, 0.0, NULL, NULL)",
                rusqlite::params![id, text, norm],
            )
            .expect("insert memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'fact', 'global_user')",
                rusqlite::params![id, text],
            )
            .expect("insert fts");
        };

        // Two memories so "distiller" is rare (df=1) → high idf.
        insert(
            "d1",
            "The distiller runs at session end and harvests durable lessons from the transcript",
        );
        insert(
            "n1",
            "Unrelated note about git rebase and squashing commits",
        );

        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &kimetsu_core::config::BrokerWeights::default(),
            ContextRequest {
                stage: "localization".to_string(),
                query: "how does the distiller work".to_string(),
                budget_tokens: 2000,
                min_lexical_coverage: 0.5,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve");

        assert!(
            bundle
                .capsules
                .iter()
                .any(|c| c.expansion_handle == "memory:d1"),
            "on-topic memory covering the rare query word must survive the floor; got: {:?}",
            bundle
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );
    }

    /// D1e-b: absolute semantic relevance floor (min_semantic_score).
    ///
    /// * With a positive floor and a query whose embedding is orthogonal
    ///   to every memory, the result must be `skipped: true` / 0 capsules.
    /// * With the same floor and a query that IS relevant, the memory
    ///   still surfaces (signal preserved).
    /// * With floor = 0.0 (default), the off-topic query still surfaces
    ///   the "best of a bad lot" (existing pre-D1e behaviour).
    #[cfg(feature = "embeddings")]
    #[test]
    fn min_semantic_score_floor_drops_off_topic_queries() {
        // DirectionalEmbedder: returns a specific unit vector based on
        // which "topic" the text is assigned to. Allows us to place the
        // query vector and memory vectors in known relative positions.
        //
        // dim=8. Topic A = [1,0,0,0,0,0,0,0]. Topic B = [0,1,0,0,0,0,0,0].
        // cosine(A, B) = 0.0 → perfectly orthogonal (unrelated).
        // cosine(A, A) = 1.0 → identical topic.
        //
        // We embed the query on topic A, the memory on topic B.
        // Cosine(query, memory) = 0.0 < any positive floor.
        struct DirectionalEmbedder {
            // Text containing "TOPIC_A" embeds as [1,0,…]; all others as [0,1,…].
            marker: &'static str,
        }
        impl embeddings::Embedder for DirectionalEmbedder {
            fn embed(&self, text: &str) -> Result<Vec<f32>, embeddings::EmbedderError> {
                let mut v = vec![0.0f32; 8];
                if text.contains(self.marker) {
                    v[0] = 1.0;
                } else {
                    v[1] = 1.0;
                }
                Ok(v)
            }
            fn model_id(&self) -> &str {
                "directional-d8"
            }
            fn dim(&self) -> usize {
                8
            }
        }

        let emb = DirectionalEmbedder { marker: "TOPIC_A" };

        let conn = rusqlite::Connection::open_in_memory().expect("in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        // Memory is on topic B (does NOT contain "TOPIC_A").
        insert_memory_with_embedding(&conn, "m_b", "cookie recipe chocolate baking TOPIC_B", &emb);

        let weights = kimetsu_core::config::BrokerWeights::default();

        // 1. Off-topic query (TOPIC_A) with a positive floor: must be skipped.
        let bundle_off = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                // Query is on TOPIC_A (cosine with memory = 0.0).
                query: "TOPIC_A unrelated phosphorescent".to_string(),
                budget_tokens: 4000,
                min_semantic_score: 0.1, // positive floor
                ..Default::default()
            },
            &[],
            &emb,
        )
        .expect("retrieve off-topic");

        assert!(
            bundle_off.capsules.is_empty(),
            "off-topic query (cosine=0 < floor=0.1) must produce zero capsules; \
             got: {:?}",
            bundle_off
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );

        // 2. On-topic query (TOPIC_B): cosine = 1.0 ≥ floor → surfaces.
        // Insert a memory explicitly on topic B that FTS can also match.
        let conn2 = rusqlite::Connection::open_in_memory().expect("in-memory 2");
        crate::schema::initialize(&conn2).expect("init schema 2");
        insert_memory_with_embedding(
            &conn2,
            "m_b2",
            "cookie recipe chocolate TOPIC_B baking"
                .to_string()
                .as_str(),
            &emb,
        );

        let bundle_on = retrieve_context_with_embedder(
            &conn2,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                // Query is on TOPIC_B: cosine with m_b2 = 1.0 ≥ floor.
                query: "cookie chocolate TOPIC_B".to_string(),
                budget_tokens: 4000,
                min_semantic_score: 0.1,
                ..Default::default()
            },
            &[],
            &emb,
        )
        .expect("retrieve on-topic");

        assert!(
            bundle_on
                .capsules
                .iter()
                .any(|c| c.expansion_handle == "memory:m_b2"),
            "on-topic query (cosine=1.0 ≥ floor) must surface m_b2; \
             got capsules: {:?}",
            bundle_on
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );

        // 3. Off-topic query with floor=0.0 (disabled): memory still surfaces
        //    (existing pre-D1e behaviour — floor is a no-op at 0.0).
        let conn3 = rusqlite::Connection::open_in_memory().expect("in-memory 3");
        crate::schema::initialize(&conn3).expect("init schema 3");
        insert_memory_with_embedding(
            &conn3,
            "m_b3",
            "cookie chocolate TOPIC_B recipe".to_string().as_str(),
            &emb,
        );

        let bundle_noop_floor = retrieve_context_with_embedder(
            &conn3,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                // FTS: "cookie chocolate" matches m_b3.
                query: "cookie chocolate TOPIC_A".to_string(),
                budget_tokens: 4000,
                min_semantic_score: 0.0, // disabled
                ..Default::default()
            },
            &[],
            &emb,
        )
        .expect("retrieve noop floor");

        // With floor disabled, FTS match is enough — memory surfaces.
        assert!(
            bundle_noop_floor
                .capsules
                .iter()
                .any(|c| c.expansion_handle == "memory:m_b3"),
            "with floor=0.0 (disabled), off-topic-cosine memory must still surface via FTS; \
             got: {:?}",
            bundle_noop_floor
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );
    }

    // ---------------------------------------------------------------
    // D1f test: token-economy reduction proof
    // ---------------------------------------------------------------

    /// D1f: Prove that embedding-MMR + semantic floor reduces token usage
    /// while preserving signal.
    ///
    /// Setup: a corpus of 6 memories:
    ///   * 3 near-duplicate paraphrases on topic A (same OracleA vector)
    ///   * 1 genuinely relevant memory on topic A (same OracleA vector,
    ///     different words)
    ///   * 2 completely unrelated memories on topic B (OracleB vector)
    ///
    /// Query: topic A.
    ///
    /// WITHOUT D1e (NoopEmbedder + floor=0.0): all 6 memories potentially
    /// surface (no semantic dedup, no floor). With the budget large enough
    /// all 6 fit → many capsules, many tokens.
    ///
    /// WITH D1e (OracleEmbedder + positive floor):
    ///   * Floor (min_semantic_score > 0) drops the 2 topic-B memories.
    ///   * Embedding-MMR collapses the 3 near-duplicate topic-A memories
    ///     to 1 slot.
    ///   * The genuinely-relevant memory survives (it is the "seed" of MMR
    ///     or at least one slot per topic-A cluster remains).
    ///
    /// Assertion: WITH D1e → strictly fewer capsules AND the genuinely-
    /// relevant memory is still present (signal preserved, noise cut).
    #[cfg(feature = "embeddings")]
    #[test]
    fn d1f_token_economy_fewer_capsules_signal_preserved() {
        // OracleEmbedder: topic-A text gets [1,0,…]; everything else [0,1,…].
        struct OracleTopicEmbedder;
        impl embeddings::Embedder for OracleTopicEmbedder {
            fn embed(&self, text: &str) -> Result<Vec<f32>, embeddings::EmbedderError> {
                let mut v = vec![0.0f32; 8];
                if text.contains("TOPIC_A") {
                    v[0] = 1.0; // topic A
                } else {
                    v[1] = 1.0; // topic B
                }
                Ok(v)
            }
            fn model_id(&self) -> &str {
                "oracle-topic-d8"
            }
            fn dim(&self) -> usize {
                8
            }
        }

        let oracle = OracleTopicEmbedder;

        // Helper: set up the corpus on a fresh connection.
        let setup = |conn: &rusqlite::Connection| {
            // 3 near-duplicate paraphrases on topic A (same oracle vector,
            // different FTS words so they match the query but Jaccard is low).
            for (mid, text) in [
                ("m_dup1", "TOPIC_A prefer ripgrep for searching"),
                ("m_dup2", "TOPIC_A rg is the fastest searcher"),
                ("m_dup3", "TOPIC_A use rg tool to find patterns"),
                // 1 genuinely-relevant memory on topic A (the one we must keep).
                (
                    "m_relevant",
                    "TOPIC_A critical lesson about search performance",
                ),
                // 2 off-topic memories on topic B.
                ("m_noise1", "chocolate cookie baking TOPIC_B recipe"),
                ("m_noise2", "gardening tulip planting TOPIC_B spring"),
            ] {
                insert_memory_with_embedding(conn, mid, text, &oracle);
            }
        };

        let weights = kimetsu_core::config::BrokerWeights::default();

        // --- WITHOUT D1e: NoopEmbedder, floor=0.0 ---
        // FTS: "TOPIC_A" appears in m_dup1/2/3 + m_relevant; "search"
        // appears in m_dup1 and m_relevant. All 4 topic-A memories match
        // FTS. The 2 topic-B memories also have "recipe" and "spring"
        // which don't match — they may or may not appear via recency
        // fallback. Use a large budget so all matching memories fit.
        let conn_lean = rusqlite::Connection::open_in_memory().expect("in-memory lean");
        crate::schema::initialize(&conn_lean).expect("init schema lean");
        setup(&conn_lean);

        let bundle_lean = retrieve_context_with_embedder(
            &conn_lean,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "TOPIC_A search performance".to_string(),
                budget_tokens: 20_000,
                min_semantic_score: 0.0, // floor disabled
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve lean");

        let lean_count = bundle_lean
            .capsules
            .iter()
            .filter(|c| c.expansion_handle.starts_with("memory:"))
            .count();

        // --- WITH D1e: OracleEmbedder + positive floor ---
        let conn_emb = rusqlite::Connection::open_in_memory().expect("in-memory emb");
        crate::schema::initialize(&conn_emb).expect("init schema emb");
        setup(&conn_emb);

        let bundle_emb = retrieve_context_with_embedder(
            &conn_emb,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "TOPIC_A search performance".to_string(),
                budget_tokens: 20_000,
                min_semantic_score: 0.5, // positive floor: drops topic-B (cosine=0.0)
                ..Default::default()
            },
            &[],
            &oracle,
        )
        .expect("retrieve with embeddings");

        let emb_count = bundle_emb
            .capsules
            .iter()
            .filter(|c| c.expansion_handle.starts_with("memory:"))
            .count();

        // Token reduction: embedding path must produce strictly fewer capsules.
        assert!(
            emb_count < lean_count,
            "D1e must reduce capsule count: embedding path {emb_count} must be \
             < lean path {lean_count}. Embedding capsules: {:?}",
            bundle_emb
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );

        // Signal preservation: the genuinely-relevant memory must survive.
        assert!(
            bundle_emb
                .capsules
                .iter()
                .any(|c| c.expansion_handle == "memory:m_relevant"),
            "m_relevant must survive D1e selection (signal preserved); \
             embedding capsules: {:?}",
            bundle_emb
                .capsules
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );

        // Token estimate: embedding path must use fewer or equal token budget.
        let lean_tokens: u32 = bundle_lean.capsules.iter().map(|c| c.token_estimate).sum();
        let emb_tokens: u32 = bundle_emb.capsules.iter().map(|c| c.token_estimate).sum();
        assert!(
            emb_tokens < lean_tokens,
            "D1e must reduce token usage: emb={emb_tokens} must be < lean={lean_tokens}"
        );
    }

    /// D1d test 4: lean-unchanged guarantee.
    ///
    /// With NoopEmbedder (query_embedding == None), memory_candidates
    /// takes the FTS-then-recency path exactly as before D1c. No vec
    /// table is touched; no panic occurs.
    #[test]
    fn lean_noop_embedder_uses_fts_then_recency_unchanged() {
        // The NoopEmbedder logic path never touches the ANN index — it must
        // work purely via FTS + recency on both lean and embeddings builds.
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        // Insert two plain memories (no embeddings).
        for (mid, text) in [
            ("m_x", "use git rebase to clean history"),
            ("m_y", "grep finds text quickly"),
        ] {
            let normalized = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score
                 )
                 VALUES (?1, 'global_user', 'fact', ?2, ?3, 1.0, NULL, '{}',
                         '2026-01-01T00:00:00Z', 0, 0.0)",
                rusqlite::params![mid, text, normalized],
            )
            .expect("insert");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope) VALUES (?1, ?2, 'fact', 'global_user')",
                rusqlite::params![mid, text],
            )
            .expect("insert fts");
        }

        let weights = kimetsu_core::config::BrokerWeights::default();
        // NoopEmbedder → query_embedding = None → FTS + recency path.
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "grep text".to_string(),
                budget_tokens: 4000,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve with NoopEmbedder must not panic");

        // m_y matches "grep text" lexically via FTS. m_x does not.
        let handles: Vec<&str> = bundle
            .capsules
            .iter()
            .filter_map(|c| c.expansion_handle.strip_prefix("memory:"))
            .collect();
        assert!(
            handles.contains(&"m_y"),
            "m_y must surface via FTS on lean path; got {handles:?}"
        );
        // Crucially: no panic, no ANN index access.
    }

    // ---------------------------------------------------------------
    // E3 tests: task-kind classification + adaptive retrieval routing
    // ---------------------------------------------------------------

    /// E3-1: classify_task is deterministic for each kind.
    #[test]
    fn classify_task_maps_each_kind_deterministically() {
        // Debug examples
        assert_eq!(
            classify_task("fix the panic in the parser"),
            TaskKind::Debug,
            "contains 'fix' and 'panic'"
        );
        assert_eq!(
            classify_task("there is a crash in auth when calling login"),
            TaskKind::Debug,
            "contains 'crash'"
        );
        assert_eq!(
            classify_task("debug the failing test"),
            TaskKind::Debug,
            "contains 'debug' and 'fail'"
        );

        // Investigation examples
        assert_eq!(
            classify_task("investigate why retrieval is slow"),
            TaskKind::Investigation,
            "contains 'investigate' and 'why'"
        );
        assert_eq!(
            classify_task("analyze the root cause of the latency"),
            TaskKind::Investigation,
            "contains 'analyze' and 'root cause'"
        );

        // Refactor examples
        assert_eq!(
            classify_task("refactor the auth module"),
            TaskKind::Refactor,
            "contains 'refactor'"
        );
        assert_eq!(
            classify_task("rename the config struct"),
            TaskKind::Refactor,
            "contains 'rename'"
        );
        assert_eq!(
            classify_task("simplify the retry handling logic"),
            TaskKind::Refactor,
            "contains 'simplify'"
        );

        // Docs examples
        assert_eq!(
            classify_task("document the API endpoints"),
            TaskKind::Docs,
            "contains 'document'"
        );
        assert_eq!(
            classify_task("update the readme with new instructions"),
            TaskKind::Docs,
            "contains 'readme'"
        );
        assert_eq!(
            classify_task("add a docstring to the main function"),
            TaskKind::Docs,
            "contains 'docstring'"
        );

        // Feature examples (default / fallback)
        assert_eq!(
            classify_task("add a dark mode toggle"),
            TaskKind::Feature,
            "no debug/refactor/docs/investigate keyword"
        );
        assert_eq!(
            classify_task("implement the new caching layer"),
            TaskKind::Feature,
            "no debug/refactor/docs/investigate keyword"
        );
        assert_eq!(
            classify_task("build the export pipeline"),
            TaskKind::Feature,
            "no debug/refactor/docs/investigate keyword"
        );
    }

    /// E3-1b: precedence — Debug > Investigation > Refactor > Docs > Feature.
    #[test]
    fn classify_task_respects_precedence_order() {
        // "fix" (Debug) + "refactor" (Refactor) → Debug wins
        assert_eq!(
            classify_task("fix and refactor the login module"),
            TaskKind::Debug,
            "Debug > Refactor"
        );
        // "investigate" (Investigation) + "refactor" (Refactor) → Investigation wins
        assert_eq!(
            classify_task("investigate and refactor the cache layer"),
            TaskKind::Investigation,
            "Investigation > Refactor"
        );
        // "investigate" (Investigation) + "document" (Docs) → Investigation wins
        assert_eq!(
            classify_task("investigate the docs and document the API"),
            TaskKind::Investigation,
            "Investigation > Docs"
        );
        // "refactor" (Refactor) + "docs" (Docs) → Refactor wins
        assert_eq!(
            classify_task("refactor and add docs"),
            TaskKind::Refactor,
            "Refactor > Docs"
        );
        // "fix" (Debug) + "investigate" (Investigation) → Debug wins
        assert_eq!(
            classify_task("fix the bug and investigate the regression"),
            TaskKind::Debug,
            "Debug > Investigation"
        );
    }

    /// E3-2: weight renormalization — weights_for_task_kind(w, Debug) sums
    /// to approximately the same total as the input weights.
    #[test]
    fn weights_for_task_kind_renormalizes_to_unit_sum() {
        let base = StageWeights {
            relevance: 0.50,
            confidence: 0.20,
            freshness: 0.20,
            scope: 0.10,
        };
        let original_sum = base.relevance + base.confidence + base.freshness + base.scope;

        for kind in [
            TaskKind::Debug,
            TaskKind::Refactor,
            TaskKind::Investigation,
            TaskKind::Docs,
        ] {
            let w = weights_for_task_kind(base.clone(), kind);
            let new_sum = w.relevance + w.confidence + w.freshness + w.scope;
            // Renormalized to 1.0; the original_sum is also 1.0 for these weights.
            assert!(
                (new_sum - original_sum).abs() < 1e-4,
                "weights_for_task_kind({kind:?}) sum {new_sum} differs from {original_sum}"
            );
        }
    }

    /// E3-2b: Feature is the neutral kind — weights unchanged.
    #[test]
    fn weights_for_task_kind_feature_is_unchanged() {
        let base = StageWeights {
            relevance: 0.40,
            confidence: 0.30,
            freshness: 0.20,
            scope: 0.10,
        };
        let w = weights_for_task_kind(base.clone(), TaskKind::Feature);
        assert!((w.relevance - base.relevance).abs() < f32::EPSILON);
        assert!((w.confidence - base.confidence).abs() < f32::EPSILON);
        assert!((w.freshness - base.freshness).abs() < f32::EPSILON);
        assert!((w.scope - base.scope).abs() < f32::EPSILON);
    }

    /// E3-2c: Debug biases toward freshness; after renorm, freshness
    /// fraction must be strictly larger than in the base weights.
    #[test]
    fn weights_for_task_kind_debug_up_freshness_fraction() {
        let base = StageWeights {
            relevance: 0.50,
            confidence: 0.20,
            freshness: 0.20,
            scope: 0.10,
        };
        let debug_w = weights_for_task_kind(base.clone(), TaskKind::Debug);
        // Freshness fraction = freshness / sum = freshness (since sum=1 after renorm).
        assert!(
            debug_w.freshness > base.freshness,
            "Debug must increase freshness fraction: {debug_w:?}"
        );
    }

    /// E3-2d: Refactor biases toward scope; after renorm, scope fraction
    /// must be strictly larger than in the base weights.
    #[test]
    fn weights_for_task_kind_refactor_up_scope_fraction() {
        let base = StageWeights {
            relevance: 0.50,
            confidence: 0.20,
            freshness: 0.20,
            scope: 0.10,
        };
        let refactor_w = weights_for_task_kind(base.clone(), TaskKind::Refactor);
        assert!(
            refactor_w.scope > base.scope,
            "Refactor must increase scope fraction: {refactor_w:?}"
        );
    }

    /// E3-3: Feature is truly neutral — retrieval with task_kind=Feature
    /// returns the same capsule set as with the default ContextRequest.
    #[test]
    fn task_kind_feature_is_retrieval_neutral() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        // Insert a few memories so retrieval has something to return.
        // DB columns: scope='project', kind=actual memory kind.
        // Broker formats summary as "{scope}:{kind} - {text}".
        for (mid, db_kind, text) in [
            ("m1", "failure_pattern", "linker not found error in build"),
            ("m2", "convention", "use snake_case for all identifiers"),
            ("m3", "fact", "the cache is invalidated on every deploy"),
        ] {
            let normalized = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score
                 )
                 VALUES (?1, 'project', ?2, ?3, ?4, 1.0, NULL, '{}',
                         '2026-01-01T00:00:00Z', 0, 0.0)",
                rusqlite::params![mid, db_kind, text, normalized],
            )
            .expect("insert memory");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, ?3, 'project')",
                rusqlite::params![mid, text, db_kind],
            )
            .expect("insert fts");
        }

        let weights = kimetsu_core::config::BrokerWeights::default();
        let query = "cache convention failure".to_string();

        // Baseline: no task_kind set (Default::default() → Feature)
        let baseline = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: query.clone(),
                budget_tokens: 4000,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("baseline retrieve");

        // Explicit Feature: must be identical to baseline
        let feature = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: query.clone(),
                budget_tokens: 4000,
                task_kind: TaskKind::Feature,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("feature retrieve");

        let baseline_ids: Vec<&str> = baseline
            .capsules
            .iter()
            .map(|c| c.expansion_handle.as_str())
            .collect();
        let feature_ids: Vec<&str> = feature
            .capsules
            .iter()
            .map(|c| c.expansion_handle.as_str())
            .collect();
        assert_eq!(
            baseline_ids, feature_ids,
            "task_kind=Feature must produce identical retrieval to default; \
             baseline={baseline_ids:?} feature={feature_ids:?}"
        );

        let baseline_scores: Vec<f32> = baseline.capsules.iter().map(|c| c.score).collect();
        let feature_scores: Vec<f32> = feature.capsules.iter().map(|c| c.score).collect();
        for (b, f) in baseline_scores.iter().zip(feature_scores.iter()) {
            assert!(
                (b - f).abs() < 1e-5,
                "scores must be identical: baseline={b} feature={f}"
            );
        }
    }

    /// E3-4: headline behavioral proof — Debug routes strictly more
    /// failure_pattern capsules than Docs over the same corpus + query.
    ///
    /// Setup: 4 failure_pattern memories + 4 convention/fact memories
    /// that all share a common topic keyword "auth". We cap at 4 capsules
    /// and compare how many are failure_pattern between Debug and Docs.
    ///
    /// Memory row layout: `scope='project'`, `kind='failure_pattern'` (or
    /// `'convention'`/`'fact'`). The broker formats the capsule summary as
    /// `"{scope}:{kind} - {text}"` so `capsule_matches_kind` can parse it.
    #[test]
    fn debug_surfaces_more_failure_pattern_than_docs() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");

        // Insert 4 failure_pattern memories.
        // DB columns: scope='project', kind='failure_pattern'
        // Broker formats summary as "project:failure_pattern - <text>".
        for (i, text) in [
            "auth token expired causes login failure",
            "auth service crash on null pointer",
            "auth regression after upgrade breaks sessions",
            "auth error when certificate is invalid",
        ]
        .iter()
        .enumerate()
        {
            let mid = format!("mfp{i}");
            let normalized = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score
                 )
                 VALUES (?1, 'project', 'failure_pattern', ?2, ?3, 1.0, NULL, '{}',
                         '2026-01-01T00:00:00Z', 0, 0.0)",
                rusqlite::params![mid, text, normalized],
            )
            .expect("insert failure_pattern");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, 'failure_pattern', 'project')",
                rusqlite::params![mid, text],
            )
            .expect("insert fts");
        }

        // Insert 4 convention/fact memories — also mention "auth".
        // DB columns: scope='project', kind='convention' or 'fact'.
        for (i, (db_kind, text)) in [
            ("convention", "auth module uses bearer tokens by convention"),
            ("convention", "auth scopes are documented in the API guide"),
            ("fact", "auth service runs on port 8443 in production"),
            ("fact", "auth uses JWT with RS256 signing for all tokens"),
        ]
        .iter()
        .enumerate()
        {
            let mid = format!("mconv{i}");
            let normalized = kimetsu_core::memory::normalize_memory_text(text);
            conn.execute(
                "INSERT INTO memories (
                     memory_id, scope, kind, text, normalized_text, confidence,
                     source_event_id, provenance_snapshot_json, created_at,
                     use_count, usefulness_score
                 )
                 VALUES (?1, 'project', ?2, ?3, ?4, 1.0, NULL, '{}',
                         '2026-01-01T00:00:00Z', 0, 0.0)",
                rusqlite::params![mid, db_kind, text, normalized],
            )
            .expect("insert convention/fact");
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope)
                 VALUES (?1, ?2, ?3, 'project')",
                rusqlite::params![mid, text, db_kind],
            )
            .expect("insert fts");
        }

        let weights = kimetsu_core::config::BrokerWeights::default();
        let query = "auth token failure".to_string();

        // Retrieve with Debug task_kind
        let debug_bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: query.clone(),
                budget_tokens: 4000,
                max_capsules: 4,
                task_kind: TaskKind::Debug,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("debug retrieve");

        // Retrieve with Docs task_kind
        let docs_bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: query.clone(),
                budget_tokens: 4000,
                max_capsules: 4,
                task_kind: TaskKind::Docs,
                ..Default::default()
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("docs retrieve");

        // Count failure_pattern capsules in each result.
        // Memory capsules have kind="memory"; the real kind is in the summary prefix.
        let count_failure_pattern = |bundle: &ContextBundle| -> usize {
            bundle
                .capsules
                .iter()
                .filter(|c| capsule_matches_kind(c, "failure_pattern"))
                .count()
        };

        let debug_fp = count_failure_pattern(&debug_bundle);
        let docs_fp = count_failure_pattern(&docs_bundle);

        assert!(
            debug_fp > docs_fp,
            "Debug must surface strictly more failure_pattern capsules than Docs: \
             debug_fp={debug_fp} docs_fp={docs_fp}\n\
             Debug capsules: {:?}\n\
             Docs capsules: {:?}",
            debug_bundle
                .capsules
                .iter()
                .map(|c| format!("{}:{}", c.kind, &c.summary[..c.summary.len().min(60)]))
                .collect::<Vec<_>>(),
            docs_bundle
                .capsules
                .iter()
                .map(|c| format!("{}:{}", c.kind, &c.summary[..c.summary.len().min(60)]))
                .collect::<Vec<_>>(),
        );
    }

    // ── F2: resolve_capsule unit tests ────────────────────────────────────

    fn init_db_with_memory(memory_id: &str, text: &str) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        let normalized = kimetsu_core::memory::normalize_memory_text(text);
        conn.execute(
            "INSERT INTO memories (
                 memory_id, scope, kind, text, normalized_text, confidence,
                 source_event_id, provenance_snapshot_json, created_at,
                 use_count, usefulness_score
             )
             VALUES (?1, 'project', 'fact', ?2, ?3, 1.0, NULL, '{}',
                     '2026-01-01T00:00:00Z', 0, 0.0)",
            rusqlite::params![memory_id, text, normalized],
        )
        .expect("insert memory");
        conn
    }

    /// F2-1: memory:<id> resolves to the full memory text.
    #[test]
    fn resolve_capsule_memory_returns_full_text() {
        let conn = init_db_with_memory("test-mem-id", "Use rg over grep for speed");
        let repo_root = std::path::Path::new("/fake-repo");
        let result =
            resolve_capsule(&conn, repo_root, "memory:test-mem-id").expect("should resolve");
        assert_eq!(result, "Use rg over grep for speed");
    }

    /// F2-2: memory:<id> for a non-existent id returns Err.
    #[test]
    fn resolve_capsule_memory_missing_id_returns_err() {
        let conn = init_db_with_memory("real-id", "some text");
        let repo_root = std::path::Path::new("/fake-repo");
        let err = resolve_capsule(&conn, repo_root, "memory:nonexistent-id")
            .expect_err("should error for missing memory");
        assert!(
            err.to_string().contains("no active memory"),
            "error message should mention missing: {err}"
        );
    }

    /// F2-3: file:<path> returns a bounded slice of the file content.
    #[test]
    fn resolve_capsule_file_returns_bounded_content() {
        let dir = make_test_dir("f2_file_resolve");
        let content = "hello from the file\n";
        std::fs::write(dir.join("notes.txt"), content).expect("write");
        let result = resolve_capsule(
            // conn is unused for file: handles; pass an in-memory DB
            &rusqlite::Connection::open_in_memory().expect("open"),
            &dir,
            "file:notes.txt",
        )
        .expect("should resolve file");
        assert!(result.contains("hello from the file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F2-4: file:<path> for a large file is capped at FILE_EXPAND_CAP_BYTES.
    #[test]
    fn resolve_capsule_file_caps_large_file() {
        let dir = make_test_dir("f2_file_cap");
        let big = "A".repeat(FILE_EXPAND_CAP_BYTES * 3);
        std::fs::write(dir.join("big.txt"), &big).expect("write");
        let result = resolve_capsule(
            &rusqlite::Connection::open_in_memory().expect("open"),
            &dir,
            "file:big.txt",
        )
        .expect("should resolve large file");
        assert!(
            result.len() <= FILE_EXPAND_CAP_BYTES + 200,
            "result should be bounded: got {} bytes",
            result.len()
        );
        assert!(
            result.contains("truncated"),
            "truncation marker should be present"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F2-5: unknown handle format returns Err.
    #[test]
    fn resolve_capsule_unknown_handle_returns_err() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        let err = resolve_capsule(&conn, std::path::Path::new("/r"), "blob:abc123")
            .expect_err("should error");
        assert!(
            err.to_string().contains("unrecognised handle"),
            "got: {err}"
        );
    }

    /// F2-6: malformed handle (no colon) returns Err.
    #[test]
    fn resolve_capsule_malformed_handle_returns_err() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        let err = resolve_capsule(&conn, std::path::Path::new("/r"), "justnocolon")
            .expect_err("should error");
        assert!(
            err.to_string().contains("unrecognised handle"),
            "got: {err}"
        );
    }

    /// F2-7: run:<id> returns the deferred-error message.
    #[test]
    fn resolve_capsule_run_handle_returns_deferred_err() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        let err = resolve_capsule(&conn, std::path::Path::new("/r"), "run:some-run-id")
            .expect_err("run: should be deferred err");
        assert!(err.to_string().contains("not yet supported"), "got: {err}");
    }

    /// F2-8: file:<path> with absolute path is rejected.
    #[test]
    fn resolve_capsule_file_rejects_absolute_path() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        let err = resolve_capsule(&conn, std::path::Path::new("/r"), "file:/etc/passwd")
            .expect_err("should reject absolute path");
        assert!(err.to_string().contains("absolute path"), "got: {err}");
    }

    // ── v1.0.0 rerank_capsules tests ─────────────────────────────────────────

    fn make_capsule(summary: &str, score: f32) -> ContextCapsule {
        ContextCapsule {
            id: new_id().to_string(),
            kind: "memory".to_string(),
            summary: summary.to_string(),
            token_estimate: 10,
            expansion_handle: format!("memory:{}", new_id()),
            provenance: vec![],
            confidence: 1.0,
            freshness: 1.0,
            relevance: 1.0,
            scope_weight: 1.0,
            score,
        }
    }

    /// RR-1: capsule whose summary shares more query words ranks first and
    /// the score field is overwritten by the reranker's sigmoid-normalized score.
    #[test]
    fn rerank_capsules_reorders_by_query_overlap() {
        use crate::embeddings::StubReranker;

        // Two capsules: "rust async tokio" shares 3/3 query tokens;
        // "python django" shares 0/3.
        let query = "rust async tokio";
        let high_overlap = make_capsule("rust async tokio runtime", 0.0);
        let low_overlap = make_capsule("python django framework", 0.0);
        // Input order: low-overlap first to verify it gets pushed down.
        let capsules = vec![low_overlap.clone(), high_overlap.clone()];

        let ranked = rerank_capsules(query, capsules, &StubReranker, 0.0, 0);

        assert_eq!(ranked.len(), 2, "both capsules should survive (floor=0)");
        // The high-overlap capsule must rank first.
        assert!(
            ranked[0].summary.contains("rust"),
            "rust capsule must be first, got: {:?}",
            ranked[0].summary
        );
        // Score must be overwritten (was 0.0, now > 0.05 for the high-overlap one).
        assert!(
            ranked[0].score > 0.05,
            "score must be overwritten by reranker: {}",
            ranked[0].score
        );
        // High-overlap must score above low-overlap.
        assert!(
            ranked[0].score > ranked[1].score,
            "high overlap must score higher: {} vs {}",
            ranked[0].score,
            ranked[1].score
        );
    }

    /// RR-2: floor drops a zero-overlap capsule.
    /// StubReranker scores a zero-overlap doc at 0.05.
    /// A floor of 0.3 must drop it.
    #[test]
    fn rerank_capsules_floor_drops_zero_overlap() {
        use crate::embeddings::StubReranker;

        let query = "rust async tokio";
        let high = make_capsule("rust async tokio runtime", 0.0);
        let zero = make_capsule("completely unrelated document xyz", 0.0); // 0-overlap → 0.05

        let capsules = vec![high, zero];
        let ranked = rerank_capsules(query, capsules, &StubReranker, 0.3, 0);

        // The zero-overlap capsule (score 0.05) must be dropped by floor=0.3.
        assert_eq!(ranked.len(), 1, "zero-overlap capsule must be dropped");
        assert!(
            ranked[0].summary.contains("rust"),
            "only rust capsule should survive"
        );
    }

    /// RR-3: cap truncates the result.
    #[test]
    fn rerank_capsules_cap_truncates() {
        use crate::embeddings::StubReranker;

        let query = "alpha beta gamma";
        let capsules = vec![
            make_capsule("alpha beta gamma delta", 0.0),
            make_capsule("alpha beta", 0.0),
            make_capsule("alpha", 0.0),
            make_capsule("unrelated xyz", 0.0),
        ];

        let ranked = rerank_capsules(query, capsules, &StubReranker, 0.0, 2);
        assert_eq!(ranked.len(), 2, "cap=2 must truncate to 2 results");
        // The top-2 should be the higher-overlap ones.
        assert!(
            ranked[0].score >= ranked[1].score,
            "results must be sorted descending"
        );
    }

    /// RR-4: fail-open — a broken reranker returns Err; input order is preserved.
    #[test]
    fn rerank_capsules_fail_open_preserves_input_order() {
        struct FailingReranker;
        impl crate::embeddings::Reranker for FailingReranker {
            fn rerank(
                &self,
                _query: &str,
                _docs: &[&str],
            ) -> Result<Vec<f32>, crate::embeddings::EmbedderError> {
                Err(crate::embeddings::EmbedderError::EmbedFailed(
                    "simulated failure".into(),
                ))
            }
            fn model_id(&self) -> &str {
                "fail-reranker"
            }
        }

        let query = "anything";
        let c1 = make_capsule("first capsule", 0.9);
        let c2 = make_capsule("second capsule", 0.5);
        let c3 = make_capsule("third capsule", 0.1);
        let capsules = vec![c1.clone(), c2.clone(), c3.clone()];

        let out = rerank_capsules(query, capsules, &FailingReranker, 0.0, 0);

        // On error: input order preserved, all 3 capsules returned.
        assert_eq!(out.len(), 3, "all capsules must be returned on error");
        assert_eq!(out[0].summary, c1.summary, "order must be preserved");
        assert_eq!(out[1].summary, c2.summary, "order must be preserved");
        assert_eq!(out[2].summary, c3.summary, "order must be preserved");
    }

    /// RR-0: empty input → empty output.
    #[test]
    fn rerank_capsules_empty_input_returns_empty() {
        use crate::embeddings::StubReranker;
        let out = rerank_capsules("query", vec![], &StubReranker, 0.0, 0);
        assert!(out.is_empty());
    }
}
