//! Shared, immutable server state (cheaply cloned per request via `Arc`).

use std::path::PathBuf;
use std::sync::Arc;

use kimetsu_chat::SkillConfig;

use crate::auth::AuthConfig;
use crate::ingest::IngestState;
use crate::metrics::Metrics;
use crate::ratelimit::RateLimiter;

#[derive(Clone)]
pub struct AppState {
    pub data_dir: Arc<PathBuf>,
    pub auth: Arc<AuthConfig>,
    pub skills: Arc<SkillConfig>,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Arc<Metrics>,
    /// Present when `--repos-file` enables server-side ingest.
    pub ingest: Option<Arc<IngestState>>,
    /// Operator-configured cross-encoder reranker for `kimetsu_brain_context`.
    /// `None` on lean builds or when `--reranker off` is passed.
    /// Wrapped in `Arc` so `AppState` stays cheaply `Clone`.
    pub reranker: Option<Arc<dyn kimetsu_brain::embeddings::Reranker>>,
}

impl AppState {
    pub fn new(data_dir: PathBuf, auth: AuthConfig) -> Self {
        Self::with_rate_limit(data_dir, auth, 0)
    }

    pub fn with_rate_limit(data_dir: PathBuf, auth: AuthConfig, per_minute: u32) -> Self {
        // Remote mode never exposes host-local skill tools, so the registry is
        // irrelevant; keep it minimal and never scan user roots on a server.
        let skills = SkillConfig {
            include_user_roots: false,
            ..SkillConfig::default()
        };
        Self {
            data_dir: Arc::new(data_dir),
            auth: Arc::new(auth),
            skills: Arc::new(skills),
            limiter: Arc::new(RateLimiter::new(per_minute)),
            metrics: Arc::new(Metrics::default()),
            ingest: None,
            reranker: None,
        }
    }

    pub fn with_ingest(mut self, ingest: Arc<IngestState>) -> Self {
        self.ingest = Some(ingest);
        self
    }

    /// Set the operator-level reranker (loaded once at startup).
    pub fn with_reranker(
        mut self,
        reranker: Option<Box<dyn kimetsu_brain::embeddings::Reranker>>,
    ) -> Self {
        self.reranker = reranker.map(|r| {
            let r: Arc<dyn kimetsu_brain::embeddings::Reranker> = Arc::from(r);
            r
        });
        self
    }
}
