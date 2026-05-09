use std::cmp::Ordering;
use std::collections::HashMap;

use kimetsu_core::config::{BrokerWeights, StageWeights};
use kimetsu_core::memory::MemoryScope;
use kimetsu_core::{KimetsuResult, ids::new_id};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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
    let mut candidates = Vec::new();
    candidates.extend(memory_candidates(conn, &request.query)?);
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

fn memory_candidates(conn: &Connection, query: &str) -> KimetsuResult<Vec<Candidate>> {
    let query_tokens = query_tokens(query);
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, scope, kind, text, confidence, created_at
        FROM memories
        ORDER BY created_at DESC
        LIMIT 200
        ",
    )?;

    let rows = stmt.query_map([], |row| {
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
        let raw_relevance = lexical_relevance(&query_tokens, &format!("{kind} {text}"));
        if raw_relevance <= 0.0 && !query_tokens.is_empty() {
            continue;
        }

        let freshness = freshness(&created_at);
        let scope_weight = scope_weight(&scope);
        candidates.push(Candidate {
            raw_relevance,
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
        });
    }
    Ok(candidates)
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

    let mut stmt = conn.prepare(
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
    let query_tokens = query_tokens(query);
    let mut stmt = conn.prepare(
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
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .map(str::trim)
        .filter(|part| part.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

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
