//! Temporary investigation harness. No production policy changes.
use kimetsu_brain::{context::ContextRequest, embeddings::{Embedder, EmbedderError, Reranker}, eval::EvalFixture, project::{self, BrainSession}, serving::ServingPolicy};
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use serde_json::json;
use std::collections::HashMap;
struct FixedVector { vector: Vec<f32>, model: String }
impl Embedder for FixedVector {
    fn embed(&self, _: &str) -> Result<Vec<f32>, EmbedderError> { Ok(self.vector.clone()) }
    fn dim(&self) -> usize { self.vector.len() }
    fn model_id(&self) -> &str { &self.model }
}
struct Scores(HashMap<String, f32>);
struct LocalReranker(std::sync::Mutex<fastembed::TextRerank>);
impl Reranker for LocalReranker {
    fn rerank(&self, q: &str, docs: &[&str]) -> Result<Vec<f32>, EmbedderError> {
        let raw = self.0.lock().unwrap().rerank(q, docs, false, None).map_err(|e| EmbedderError::EmbedFailed(e.to_string()))?;
        let mut scores = vec![f32::NAN; docs.len()];
        for x in raw { scores[x.index] = 1.0 / (1.0 + (-x.score).exp()); }
        assert!(scores.iter().all(|v| v.is_finite())); Ok(scores)
    }
    fn model_id(&self) -> &str { "mmarco-multilingual-avx2-diagnostic" }
}
impl Reranker for Scores {
    fn rerank(&self, _: &str, docs: &[&str]) -> Result<Vec<f32>, EmbedderError> {
        docs.iter().map(|d| self.0.get(*d).copied().ok_or_else(|| EmbedderError::EmbedFailed("missing diagnostic score".into()))).collect()
    }
    fn model_id(&self) -> &str { "cached-diagnostic-scores" }
}
fn main() -> kimetsu_core::KimetsuResult<()> {
    let args: Vec<String> = std::env::args().collect();
    let fixture: EvalFixture = serde_json::from_str(&std::fs::read_to_string(&args[1])?)?;
    let emb = kimetsu_brain::embeddings::open_embedder_for_model("bge-small-en-v1.5");
    assert!(!emb.is_noop());
    let rr: Box<dyn Reranker> = if let Ok(path) = std::env::var("PROBE_RERANKER_PATH") {
        let p = std::path::Path::new(&path);
        let files = fastembed::TokenizerFiles {
            tokenizer_file: std::fs::read(p.join("tokenizer.json"))?,
            config_file: std::fs::read(p.join("config.json"))?,
            tokenizer_config_file: std::fs::read(p.join("tokenizer_config.json"))?,
            special_tokens_map_file: std::fs::read(p.join("special_tokens_map.json"))?,
        };
        let model = fastembed::UserDefinedRerankingModel::new(fastembed::OnnxSource::File(p.join("onnx/model_quint8_avx2.onnx")), files);
        Box::new(LocalReranker(std::sync::Mutex::new(fastembed::TextRerank::try_new_from_user_defined(model, Default::default())?)))
    } else { kimetsu_brain::embeddings::open_reranker_checked("ms-marco-tinybert-l-2-v2")?.unwrap() };
    let tmp = tempfile::tempdir()?;
    kimetsu_core::paths::git_init_boundary(tmp.path());
    project::init_project(tmp.path(), true)?;
    let mut keys = HashMap::new();
    for mem in &fixture.memories {
        let id = project::add_memory(tmp.path(), MemoryScope::Project, MemoryKind::Fact, &mem.text)?;
        keys.insert(format!("memory:{id}"), mem.key.clone());
    }
    let session = BrainSession::open_readonly(tmp.path())?;
    let mut results = Vec::new();
    for (i, case) in fixture.cases.iter().enumerate() {
        let fixed = FixedVector { vector: emb.embed(&case.query)?, model: emb.model_id().into() };
        let mut req = ContextRequest { query: case.query.clone(), stage: "localization".into(), ..Default::default() };
        session.resolve_request_floors(&mut req);
        let mut variants = Vec::new();
        let production_only = std::env::var_os("PROBE_PRODUCTION_ONLY").is_some();
        for pool in if production_only { vec![6] } else { vec![6,16,32] } {
            let policy = ServingPolicy { pool, cap: 4, ..Default::default() };
            variants.push((format!("pool-{pool}"), policy, session.retrieve_context_with_injected_embedder(policy.prepare(req.clone(), true), &fixed)?));
        }
        let mut raw = req.clone();
        raw.min_semantic_score_override = Some(0.0); raw.min_lexical_coverage_override = Some(0.0); raw.abstain_evidence_override = Some(0.0);
        raw.budget_tokens = 64_000; raw.max_capsules = 32;
        if !production_only { variants.push(("ungated-32".into(), ServingPolicy { pool:32, cap:4, ..Default::default() }, session.retrieve_context_with_injected_embedder(raw, &fixed)?)); }
        let mut docs: Vec<String> = variants.iter().flat_map(|(_,_,b)| b.capsules.iter().chain(b.excluded.iter().filter(|_| !production_only)).map(|c| c.summary.clone())).collect();
        docs.sort(); docs.dedup();
        let values = rr.rerank(&case.query, &docs.iter().map(String::as_str).collect::<Vec<_>>())?;
        assert_eq!(docs.len(), values.len());
        let scores = Scores(docs.into_iter().zip(values).collect());
        let mut stages = Vec::new();
        for (name, policy, bundle) in variants {
            let candidates: Vec<_> = bundle.capsules.iter().map(|c| json!({"key":keys.get(&c.expansion_handle),"ce":scores.0[&c.summary],"rank_score":c.score,"text":c.summary})).collect();
            let excluded_gold: Vec<_> = bundle.excluded.iter().filter(|c| keys.get(&c.expansion_handle).is_some_and(|k| case.relevant.contains(k))).map(|c| json!({"key":keys.get(&c.expansion_handle),"ce":scores.0.get(&c.summary)})).collect();
            let top = bundle.top_abs_evidence;
            let skipped = bundle.skipped;
            let out = policy.arbitrate(&case.query, bundle, Some(&scores), req.abstain_evidence);
            let delivered = policy.render(out, session.config().broker.compress_capsules, kimetsu_brain::serving::EVAL_EXPOSURE_ID);
            stages.push(json!({"name":name,"top_cosine":top,"pre_skipped":skipped,"candidates":candidates,"excluded_gold":excluded_gold,"delivered":delivered.capsules.iter().map(|c|keys.get(&c.expansion_handle)).collect::<Vec<_>>()}));
        }
        results.push(json!({"query":case.query,"relevant":case.relevant,"floors":{"semantic":req.min_semantic_score,"lexical":req.min_lexical_coverage,"abstain":req.abstain_evidence},"stages":stages}));
        std::fs::write(&args[2], serde_json::to_vec_pretty(&results)?)?;
        eprintln!("probe {}/{}", i+1, fixture.cases.len());
    }
    Ok(())
}
