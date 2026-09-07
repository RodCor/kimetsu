//! Canonical brain-context retrieval and final MCP delivery policy. Evaluators
//! supply the same request/configuration and measure this complete renderer.
use crate::context::{
    ContextBundle, ContextRequest,
    delivery::{Delivery, compact_capsules, fit_json},
    rerank_and_arbitrate,
};
use crate::embeddings::{Embedder, Reranker};
use crate::project::BrainSession;
use kimetsu_core::KimetsuResult;
use serde_json::json;

pub const RERANK_POOL: usize = 6;
/// A score threshold, not a calibrated relevance probability.
pub const RERANK_FLOOR: f32 = 0.30;
pub const DEFAULT_BUDGET: u32 = 6000;
pub const DEFAULT_CAP: usize = 3;
/// ULIDs on the serving surface have exactly this length. Evaluation does not
/// persist an exposure and therefore uses a deterministic placeholder.
pub const EVAL_EXPOSURE_ID: &str = "00000000000000000000000000";

/// Precompute once so a low-level fallback cannot disguise failed inference.
struct QueryVector<'a> {
    inner: &'a dyn Embedder,
    vector: Vec<f32>,
}
impl Embedder for QueryVector<'_> {
    fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embeddings::EmbedderError> {
        Ok(self.vector.clone())
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
}
struct CheckedScores<'a> {
    model: &'a str,
    scores: Vec<f32>,
}
impl Reranker for CheckedScores<'_> {
    fn rerank(&self, _: &str, _: &[&str]) -> Result<Vec<f32>, crate::embeddings::EmbedderError> {
        Ok(self.scores.clone())
    }
    fn model_id(&self) -> &str {
        self.model
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ServingPolicy {
    pub budget: u32,
    pub cap: usize,
    pub pool: usize,
    pub rerank_floor: f32,
    pub explicit_fact_guard: bool,
}
impl Default for ServingPolicy {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            cap: DEFAULT_CAP,
            pool: RERANK_POOL,
            rerank_floor: RERANK_FLOOR,
            explicit_fact_guard: false,
        }
    }
}
impl ServingPolicy {
    pub fn from_config(config: &kimetsu_core::config::ProjectConfig) -> Self {
        Self {
            rerank_floor: config.broker.rerank_min_score,
            explicit_fact_guard: config.broker.explicit_fact_guard,
            ..Self::default()
        }
    }
    pub fn prepare(&self, mut request: ContextRequest, reranking: bool) -> ContextRequest {
        request.budget_tokens = if reranking {
            self.budget.max(DEFAULT_BUDGET)
        } else {
            self.budget
        };
        request.max_capsules = if reranking {
            self.cap.max(self.pool)
        } else {
            self.cap
        };
        request
    }
    pub fn arbitrate(
        &self,
        query: &str,
        bundle: ContextBundle,
        reranker: Option<&dyn Reranker>,
        abstain: f32,
    ) -> ContextBundle {
        let mut bundle = rerank_and_arbitrate(
            query,
            bundle,
            reranker,
            abstain,
            self.rerank_floor,
            if self.explicit_fact_guard {
                0
            } else {
                self.cap
            },
        );
        if self.explicit_fact_guard {
            crate::answerability::filter_bundle(query, &mut bundle);
        }
        if self.cap > 0 {
            bundle.capsules.truncate(self.cap);
        }
        bundle.used_tokens = bundle.capsules.iter().map(|c| c.token_estimate).sum();
        if bundle.capsules.is_empty() {
            bundle.skipped = true;
            bundle.evidence_coverage = 0.0;
        }
        bundle
    }
    pub fn render_for_query(
        &self,
        query: &str,
        mut bundle: ContextBundle,
        compress: bool,
        exposure_id: &str,
    ) -> Delivery {
        if compress {
            for capsule in &mut bundle.capsules {
                capsule.summary = if self.explicit_fact_guard {
                    crate::answerability::compress_preserving_evidence(query, &capsule.summary, 3)
                } else {
                    crate::context::compress_for_render(&capsule.summary, 3)
                };
            }
        }
        self.render(bundle, false, exposure_id)
    }
    pub fn render(&self, mut bundle: ContextBundle, compress: bool, exposure_id: &str) -> Delivery {
        if compress {
            for c in &mut bundle.capsules {
                c.summary = crate::context::compress_for_render(&c.summary, 3)
            }
        }
        let count = bundle.capsules.len();
        fit_json(bundle.capsules.clone(), self.budget, |capsules| {
            json!({
                "ok":true,"skipped":capsules.is_empty(),"exposure_id":exposure_id,
                "capsule_count":capsules.len(),"excluded_count":bundle.excluded.len()+count-capsules.len(),
                "capsules":compact_capsules(capsules),"partial_evidence":bundle.evidence_coverage<1.0 || capsules.len()<count,
            })
        })
    }
    pub fn retrieve(
        &self,
        session: &BrainSession,
        mut request: ContextRequest,
        embedder: &dyn Embedder,
        reranker: Option<&dyn Reranker>,
        exposure_id: &str,
    ) -> KimetsuResult<Delivery> {
        session.resolve_request_floors(&mut request);
        let abstain = request.abstain_evidence;
        let query = request.query.clone();
        let query_vector = if embedder.is_noop() {
            None
        } else {
            let vector = embedder.embed(&query)?;
            if vector.len() != embedder.dim()
                || vector.is_empty()
                || vector.iter().any(|x| !x.is_finite())
                || !vector.iter().any(|x| *x != 0.0)
            {
                return Err(
                    "embedder returned an invalid query vector; no semantic measurement".into(),
                );
            }
            Some(QueryVector {
                inner: embedder,
                vector,
            })
        };
        let checked_embedder = query_vector
            .as_ref()
            .map(|v| v as &dyn Embedder)
            .unwrap_or(embedder);
        let bundle = session.retrieve_context_with_injected_embedder(
            self.prepare(request, reranker.is_some()),
            checked_embedder,
        )?;
        let checked_scores = if let Some(rr) = reranker.filter(|_| !bundle.capsules.is_empty()) {
            let docs: Vec<_> = bundle.capsules.iter().map(|c| c.summary.as_str()).collect();
            let scores = rr.rerank(&query, &docs)?;
            if scores.len() != docs.len()
                || scores
                    .iter()
                    .any(|s| !s.is_finite() || !(0.0..=1.0).contains(s))
            {
                return Err(
                    "reranker returned invalid scores; no cross-encoder measurement".into(),
                );
            }
            Some(CheckedScores {
                model: rr.model_id(),
                scores,
            })
        } else {
            None
        };
        let bundle = self.arbitrate(
            &query,
            bundle,
            checked_scores.as_ref().map(|r| r as &dyn Reranker),
            abstain,
        );
        Ok(self.render_for_query(
            &query,
            bundle,
            session.config().broker.compress_capsules,
            exposure_id,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        context::ContextCapsule,
        embeddings::{StubEmbedder, StubReranker},
        project,
    };
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    struct FailedEmbedder;
    impl Embedder for FailedEmbedder {
        fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embeddings::EmbedderError> {
            Err(crate::embeddings::EmbedderError::EmbedFailed(
                "test failure".into(),
            ))
        }
        fn model_id(&self) -> &str {
            "failed"
        }
        fn dim(&self) -> usize {
            2
        }
    }
    struct MalformedReranker;
    impl Reranker for MalformedReranker {
        fn rerank(
            &self,
            _: &str,
            _: &[&str],
        ) -> Result<Vec<f32>, crate::embeddings::EmbedderError> {
            Ok(vec![])
        }
        fn model_id(&self) -> &str {
            "malformed"
        }
    }
    #[test]
    fn production_and_eval_use_same_final_budget_and_arbitration_with_injected_models() {
        crate::user_brain::with_user_brain_disabled(|| {
            let root = std::env::temp_dir().join(format!("kimetsu-serving-{}", ulid::Ulid::new()));
            kimetsu_core::paths::git_init_boundary(&root);
            project::init_project(&root, false).unwrap();
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "wal checkpoint protects sqlite commits",
            )
            .unwrap();
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "remote network bandwidth compression",
            )
            .unwrap();
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).unwrap();
            let mut config = project::load_config(&paths).unwrap();
            config.broker.min_semantic_score = 0.73;
            std::fs::write(&paths.project_toml, config.to_toml().unwrap()).unwrap();
            let session = BrainSession::open_readonly(&root).unwrap();
            let embedder = StubEmbedder::default();
            let rr = StubReranker;
            let request = ContextRequest {
                query: "wal checkpoint".into(),
                stage: "localization".into(),
                ..Default::default()
            };
            assert!(
                ServingPolicy::default()
                    .retrieve(
                        &session,
                        request.clone(),
                        &FailedEmbedder,
                        None,
                        EVAL_EXPOSURE_ID
                    )
                    .is_err(),
                "failed inference cannot become successful semantic measurement"
            );
            assert!(
                ServingPolicy::default()
                    .retrieve(
                        &session,
                        request,
                        &embedder,
                        Some(&MalformedReranker),
                        EVAL_EXPOSURE_ID
                    )
                    .is_err(),
                "malformed CE cannot become successful reranker measurement"
            );
            for budget in [1, 600, 1200, 6000] {
                let policy = ServingPolicy {
                    budget,
                    ..Default::default()
                };
                let req = ContextRequest {
                    query: "wal checkpoint".into(),
                    stage: "localization".into(),
                    min_score: 0.15,
                    ..Default::default()
                };
                let eval = policy
                    .retrieve(
                        &session,
                        req.clone(),
                        &embedder,
                        Some(&rr),
                        EVAL_EXPOSURE_ID,
                    )
                    .unwrap();
                let mut resolved = req;
                session.resolve_request_floors(&mut resolved);
                let abstain = resolved.abstain_evidence;
                let bundle = session
                    .retrieve_context_with_injected_embedder(
                        policy.prepare(resolved, true),
                        &embedder,
                    )
                    .unwrap();
                let production = policy.render(
                    policy.arbitrate("wal checkpoint", bundle, Some(&rr), abstain),
                    false,
                    EVAL_EXPOSURE_ID,
                );
                let mut produced = production.payload.clone();
                let mut measured = eval.payload.clone();
                for payload in [&mut produced, &mut measured] {
                    if let Some(caps) = payload["capsules"].as_array_mut() {
                        for c in caps {
                            c["id"] = json!(EVAL_EXPOSURE_ID);
                        }
                    }
                }
                assert_eq!(produced, measured);
                assert_eq!(
                    eval.payload["used_tokens"],
                    json!(crate::context::delivery::serialized_output_tokens(
                        &eval.payload
                    ))
                );
                if budget == 1 {
                    assert!(eval.capsules.is_empty());
                    assert_eq!(eval.payload["error"], "budget_too_small");
                }
            }
            let mut request = ContextRequest::default();
            session.resolve_request_floors(&mut request);
            assert_eq!(request.min_semantic_score, 0.73);
            request.min_semantic_score_override = Some(0.0);
            session.resolve_request_floors(&mut request);
            assert_eq!(request.min_semantic_score, 0.0);
            request.min_semantic_score_override = Some(-1.0);
            session.resolve_request_floors(&mut request);
            assert!(matches!(request.min_semantic_score, 0.35 | 0.0));
        });
    }
    #[test]
    fn compression_keeps_the_value_that_justified_admission() {
        let capsule = ContextCapsule::wire_minimal(
            "Background one. Background two. Background three. password = `test-value-only`".into(),
            "memory".into(),
            0.99,
        );
        let bundle = ContextBundle {
            stage: "localization".into(),
            budget_tokens: 6000,
            used_tokens: 0,
            capsules: vec![capsule],
            excluded: vec![],
            skipped: false,
            top_score: 0.99,
            top_abs_evidence: 0.99,
            evidence_coverage: 1.0,
            uncovered_terms: vec![],
            chronological: false,
        };
        let policy = ServingPolicy {
            explicit_fact_guard: true,
            ..Default::default()
        };
        let delivered =
            policy.render_for_query("What is the password?", bundle, true, EVAL_EXPOSURE_ID);
        assert!(delivered.payload.to_string().contains("test-value-only"));
    }
    #[test]
    fn explicit_fact_guard_excludes_topic_match_before_output_cap() {
        let capsules = [
            "The listener binds port 6319.",
            "password = `test-value-only`",
        ]
        .into_iter()
        .map(|text| ContextCapsule::wire_minimal(text.into(), "memory".into(), 0.99))
        .collect();
        let bundle = ContextBundle {
            stage: "localization".into(),
            budget_tokens: 6000,
            used_tokens: 0,
            capsules,
            excluded: vec![],
            skipped: false,
            top_score: 0.99,
            top_abs_evidence: 0.99,
            evidence_coverage: 1.0,
            uncovered_terms: vec![],
            chronological: false,
        };
        let policy = ServingPolicy {
            cap: 1,
            explicit_fact_guard: true,
            ..Default::default()
        };
        let selected = policy.arbitrate("What password is required?", bundle, None, 0.0);
        assert_eq!(selected.capsules.len(), 1);
        assert!(selected.capsules[0].summary.contains("test-value-only"));
        assert_eq!(selected.excluded.len(), 1);
    }

    #[test]
    fn reranker_floor_and_final_serialization_reject_candidates_before_measurement() {
        let capsules = [("hit", "wal checkpoint"), ("noise", "remote network")]
            .into_iter()
            .map(|(id, text)| {
                let mut c = ContextCapsule::wire_minimal(text.into(), "memory".into(), 0.8);
                c.id = id.into();
                c.expansion_handle = format!("memory:{id}");
                c
            })
            .collect();
        let bundle = ContextBundle {
            stage: "localization".into(),
            budget_tokens: 6000,
            used_tokens: 0,
            capsules,
            excluded: vec![],
            skipped: false,
            top_score: 0.8,
            top_abs_evidence: -1.0,
            evidence_coverage: 1.0,
            uncovered_terms: vec![],
            chronological: false,
        };
        let policy = ServingPolicy::default();
        let selected = policy.arbitrate("wal checkpoint", bundle, Some(&StubReranker), 0.0);
        assert_eq!(selected.capsules.len(), 1);
        assert_eq!(selected.capsules[0].id, "hit");
        let delivery = policy.render(selected, false, EVAL_EXPOSURE_ID);
        assert_eq!(delivery.capsules.len(), 1);
        assert!(!delivery.payload.to_string().contains("remote network"));
    }
}
