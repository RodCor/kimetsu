use serde::{Deserialize, Serialize};

use crate::{KIMETSU_CONFIG_VERSION, KimetsuResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub kimetsu: KimetsuSection,
    pub model: ModelSection,
    pub broker: BrokerSection,
    pub shell: ShellSection,
    pub ingestion: IngestionSection,
    pub run: RunSection,
    /// v0.8: which built-in embedding model the brain uses. The
    /// `#[serde(default)]` keeps every pre-v0.8 project.toml loading
    /// cleanly (they get the lean English default). Resolution
    /// precedence is `KIMETSU_BRAIN_EMBEDDER` env > this field >
    /// default; see `kimetsu_brain::embeddings::resolve_embedder_id`.
    #[serde(default)]
    pub embedder: EmbedderSection,
    /// v0.8.5: automatic memory harvesting. `#[serde(default)]` keeps
    /// pre-v0.8.5 project.toml files loading cleanly (they get
    /// auto-harvest on).
    #[serde(default)]
    pub learning: LearningSection,
    /// S1.2: top-level cheap-model override. When present, takes
    /// precedence over `[learning.distiller]` as the resolved cheap
    /// model for all consumers (distiller, consolidation, future
    /// digest/ask). Entirely optional — when absent the resolver falls
    /// back to `[learning.distiller]` for back-compat. `#[serde(default)]`
    /// keeps all existing project.toml files loading unchanged.
    #[serde(default)]
    pub cheap_model: Option<CheapModelSection>,
    /// S5.1: storage / retrieval backend selection. `#[serde(default)]`
    /// keeps every existing project.toml loading cleanly (they get
    /// `backend = "flat"`, the current FTS + usearch-ANN path).
    #[serde(default)]
    pub storage: StorageSection,
    /// Epic S3: personal brain sync via event-log replication.
    /// `#[serde(default)]` keeps every pre-S3 project.toml loading cleanly
    /// (they get no sync dir and a freshly-generated machine_id).
    #[serde(default)]
    pub sync: SyncSection,
    /// F3 Flagship 3 / Lifecycle & forgetting policy.
    /// `#[serde(default)]` keeps all existing project.toml files loading
    /// cleanly (they get forgetting disabled, sane defaults for all thresholds,
    /// regret threshold 5, proposal expiry 30d, auto-accept disabled).
    #[serde(default)]
    pub lifecycle: LifecycleSection,
}

impl ProjectConfig {
    pub fn default_for_project(project_id: impl Into<String>) -> Self {
        Self {
            kimetsu: KimetsuSection {
                project_id: project_id.into(),
                schema_version: KIMETSU_CONFIG_VERSION,
                use_user_brain: default_true(),
                mcp_write_tools: default_true(),
            },
            model: ModelSection::default(),
            broker: BrokerSection::default(),
            shell: ShellSection::default(),
            ingestion: IngestionSection::default(),
            run: RunSection::default(),
            embedder: EmbedderSection::default(),
            learning: LearningSection::default(),
            cheap_model: None,
            storage: StorageSection::default(),
            sync: SyncSection::default(),
            lifecycle: LifecycleSection::default(),
        }
    }

    /// S1.2: resolve the effective cheap-model config.
    ///
    /// Resolution order (first wins):
    ///   1. `[cheap_model]` if present AND `enabled = true` — explicit top-level
    ///      section introduced in S1.2.
    ///   2. `[learning.distiller]` if `enabled = true` — back-compat alias so
    ///      any existing config with `[learning.distiller]` keeps working with
    ///      zero changes.
    ///   3. `None` — no cheap model configured; consumers degrade gracefully
    ///      (no panic, feature just does not run — same as distiller-absent
    ///      behaviour before S1.2).
    ///
    /// FUTURE consumers (digest/resume/skill/ask) must call this resolver so
    /// resolution stays in ONE place.
    pub fn cheap_model(&self) -> Option<CheapModelSection> {
        if let Some(ref cm) = self.cheap_model {
            if cm.enabled {
                return Some(cm.clone());
            }
        }
        // Back-compat: treat an enabled [learning.distiller] as the cheap model.
        if self.learning.distiller.enabled {
            return Some(CheapModelSection::from_distiller(&self.learning.distiller));
        }
        None
    }

    pub fn from_toml(value: &str) -> KimetsuResult<Self> {
        Ok(toml::from_str(value)?)
    }

    pub fn to_toml(&self) -> KimetsuResult<String> {
        Ok(toml::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KimetsuSection {
    pub project_id: String,
    pub schema_version: i64,
    /// W3.3: per-project opt-out of the global cross-project user brain
    /// (`~/.kimetsu/brain.db`). When false, GlobalUser writes fall back to
    /// the project DB and retrieval skips the user-brain merge — identical
    /// to `KIMETSU_USER_BRAIN=0` but durable and scoped to this project.
    ///
    /// Precedence: `KIMETSU_USER_BRAIN` env > this field > default (true).
    /// `#[serde(default)]` keeps all pre-W3 project.toml files loading
    /// unchanged (they get `use_user_brain = true`).
    #[serde(default = "default_true")]
    pub use_user_brain: bool,
    /// v1.0.0: allow the LOCAL stdio MCP server to expose privileged write
    /// tools (`kimetsu_brain_record`, `memory_add/accept/reject`, …).
    /// Default true: the local plugin install on your own machine is the
    /// "trusted session" the gate exists for, and the brain's own workflow
    /// (CLAUDE.md guidance, the Stop-hook harvest cue) instructs the agent
    /// to record lessons — a default-deny gate contradicted that at every
    /// session end. Personalize via `kimetsu config set
    /// kimetsu.mcp_write_tools false`. Precedence:
    /// `KIMETSU_MCP_ENABLE_WRITE_TOOLS` env (set = wins, truthy/falsy) >
    /// this field > default (true). The REMOTE server ignores this field
    /// entirely (a cloned repo's project.toml is untrusted there) and
    /// stays env-only, default-deny.
    #[serde(default = "default_true")]
    pub mcp_write_tools: bool,
}

/// v0.8: embedding-model selection. `model` is one of the curated
/// built-in ids exposed by `kimetsu brain model list`
/// (`bge-small-en-v1.5`, `bge-m3`, `jina-v2-base-code`). Switching
/// changes the vector dimension, so a `kimetsu brain reindex` is
/// required for cosine retrieval to use the new model.
///
/// W3.1: `enabled` is a persistent off-switch for the embedding engine.
/// When false, the embedder resolves to NoopEmbedder (FTS-only; no
/// vectors written or queried). Precedence: `KIMETSU_BRAIN_EMBEDDER`
/// env override > this field > default (true). A disable env value
/// (`noop`/`off`/`0`/…) always wins; a real model-id env value means
/// "enabled" regardless of this field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedderSection {
    #[serde(default = "default_embedder_id")]
    pub model: String,
    /// W3.1: persistent embeddings off-switch. Default true (enabled).
    /// `#[serde(default = "default_true")]` keeps pre-W3 project.toml
    /// files loading unchanged.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// v1.0.0: use the warm embedder daemon for the `UserPromptSubmit`
    /// hook. `false` ⇒ the hook never spawns/contacts a daemon and stays
    /// on floored-FTS even on `embeddings` builds. Config equivalent of
    /// `KIMETSU_EMBED_DAEMON=0`. `#[serde(default = "default_true")]`
    /// keeps older configs loading with the daemon on.
    #[serde(default = "default_true")]
    pub daemon: bool,
    /// v1.0.0: pre-warm the daemon at harness startup (via `kimetsu brain
    /// warm`, wired to SessionStart). `false` ⇒ no startup spawn; the
    /// daemon (if `daemon=true`) warms lazily on the first prompt instead.
    #[serde(default = "default_true")]
    pub warm_on_start: bool,
    /// v1.0.0: cross-encoder reranker the warm daemon applies as the final
    /// ranking stage. `"off"` (default), a curated fastembed reranker id
    /// (`jina-reranker-v1-turbo-en`, `bge-reranker-base`,
    /// `bge-reranker-v2-m3`, `jina-reranker-v2-base-multilingual`), a
    /// benchmarked alias (`jina-reranker-v1-tiny-en`,
    /// `ms-marco-tinybert-l-2-v2`, `ms-marco-minilm-l-4-v2`), or any
    /// HuggingFace `org/repo` with an ONNX export.
    ///
    /// Default `ms-marco-tinybert-l-2-v2`, chosen with `kimetsu brain
    /// bench` on the 100-case real-memory dataset: paired with the
    /// `jina-v2-base-code` embedder it lands within noise of the best
    /// quality (MRR 0.938 vs 0.953 top) at ~43ms per rerank — far inside
    /// the hook's 300ms budget. On slower machines a miss degrades
    /// gracefully to floored-FTS for that turn. `"off"` disables
    /// reranking. Validate changes on your own corpus with
    /// `kimetsu brain bench` / `kimetsu brain eval`.
    #[serde(default = "default_reranker_id")]
    pub reranker: String,
}

fn default_embedder_id() -> String {
    "jina-v2-base-code".to_string()
}

fn default_reranker_id() -> String {
    "ms-marco-tinybert-l-2-v2".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for EmbedderSection {
    fn default() -> Self {
        Self {
            model: default_embedder_id(),
            enabled: default_true(),
            daemon: default_true(),
            warm_on_start: default_true(),
            reranker: default_reranker_id(),
        }
    }
}

/// v0.8.5: automatic memory harvesting. When `auto_harvest` is on, the
/// proactive PostToolUse hook and the Stop hook emit a `[kimetsu-harvest]`
/// cue at high-signal moments (a failed-then-fixed command, or a
/// non-trivial session that recorded nothing) telling the agent to
/// dispatch the `kimetsu-memory-harvester` subagent. Set it false to
/// silence those cues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningSection {
    #[serde(default = "default_auto_harvest")]
    pub auto_harvest: bool,
    /// v1.5: store the raw retrieval query in local `context.served`
    /// telemetry so the self-tuning loop can build a personal eval set.
    /// Data never leaves the machine and is never exported. Set false to
    /// keep only the query hash (the pre-v1.5 behavior). Default true so
    /// new installs gain the eval-set signal immediately on upgrade;
    /// `#[serde(default = "default_true")]` keeps pre-v1.5 project.toml
    /// files loading cleanly (they get store_queries = true).
    #[serde(default = "default_true")]
    pub store_queries: bool,
    /// Opt-in credentialed SessionEnd distiller (configured by the install
    /// wizard). Disabled by default; `#[serde(default)]` keeps older
    /// project.toml files loading.
    #[serde(default)]
    pub distiller: DistillerSection,
}

fn default_auto_harvest() -> bool {
    true
}

impl Default for LearningSection {
    fn default() -> Self {
        Self {
            auto_harvest: default_auto_harvest(),
            store_queries: default_true(),
            distiller: DistillerSection::default(),
        }
    }
}

/// Credentialed SessionEnd distiller config. Secret values (the API key,
/// optional base URL) live in `.env` under the env-var names below; only
/// non-secret selection lives here. `provider` is `anthropic`, `openai`, or
/// `bedrock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistillerSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_distiller_provider")]
    pub provider: String,
    #[serde(default = "default_distiller_model")]
    pub model: String,
    #[serde(default = "default_distiller_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_distiller_base_url_env")]
    pub base_url_env: String,
    /// AWS Bedrock distiller: literal region. Takes precedence over
    /// `region_env`. `#[serde(default)]` keeps existing config loading.
    #[serde(default)]
    pub region: Option<String>,
    /// AWS Bedrock distiller: env-var name that holds the region.
    /// Defaults to `"AWS_REGION"`. `#[serde(default)]` keeps existing
    /// config loading cleanly.
    #[serde(default = "default_distiller_region_env")]
    pub region_env: String,
}

fn default_distiller_provider() -> String {
    "anthropic".to_string()
}
fn default_distiller_model() -> String {
    "claude-haiku-4-5".to_string()
}
fn default_distiller_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}
fn default_distiller_base_url_env() -> String {
    "ANTHROPIC_BASE_URL".to_string()
}

fn default_distiller_region_env() -> String {
    "AWS_REGION".to_string()
}

impl Default for DistillerSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_distiller_provider(),
            model: default_distiller_model(),
            api_key_env: default_distiller_api_key_env(),
            base_url_env: default_distiller_base_url_env(),
            region: None,
            region_env: default_distiller_region_env(),
        }
    }
}

/// S1.2: top-level cheap-model config. Same shape as `DistillerSection`
/// (provider / model / api_key_env / base_url_env / region / region_env) but
/// with `provider` now including `"ollama"` (S1.1).
///
/// Providers:
///   - `"anthropic"` / `"claude"` — Anthropic API.
///   - `"openai"` / `"gpt"` / `"oai"` — OpenAI-compatible API.
///   - `"ollama"` — local Ollama server (OpenAI-compatible at
///     `http://localhost:11434/v1`). No API key required. Recommended
///     small instruct models: `qwen2.5:3b`, `llama3.2:3b`.
///     Override the endpoint with `base_url_env` (default env var
///     `OLLAMA_BASE_URL`).
///   - `"bedrock"` / `"aws"` — AWS Bedrock.
///
/// `#[serde(default)]` on the `ProjectConfig` field keeps all existing
/// project.toml files loading unchanged (`cheap_model = None`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheapModelSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_cheap_model_provider")]
    pub provider: String,
    #[serde(default = "default_cheap_model_model")]
    pub model: String,
    /// Env-var name that holds the API key (not required for `ollama`).
    #[serde(default = "default_cheap_model_api_key_env")]
    pub api_key_env: String,
    /// Env-var name that holds the base URL override. For `ollama` the
    /// default resolved URL is `http://localhost:11434/v1` when this env
    /// var is absent or empty.
    #[serde(default = "default_cheap_model_base_url_env")]
    pub base_url_env: String,
    /// AWS Bedrock: literal region. Takes precedence over `region_env`.
    #[serde(default)]
    pub region: Option<String>,
    /// AWS Bedrock: env-var name that holds the region (default `"AWS_REGION"`).
    #[serde(default = "default_cheap_model_region_env")]
    pub region_env: String,
}

fn default_cheap_model_provider() -> String {
    "anthropic".to_string()
}
fn default_cheap_model_model() -> String {
    "claude-haiku-4-5".to_string()
}
fn default_cheap_model_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}
fn default_cheap_model_base_url_env() -> String {
    "ANTHROPIC_BASE_URL".to_string()
}
fn default_cheap_model_region_env() -> String {
    "AWS_REGION".to_string()
}

impl Default for CheapModelSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_cheap_model_provider(),
            model: default_cheap_model_model(),
            api_key_env: default_cheap_model_api_key_env(),
            base_url_env: default_cheap_model_base_url_env(),
            region: None,
            region_env: default_cheap_model_region_env(),
        }
    }
}

impl CheapModelSection {
    /// S1.1: for `provider = "ollama"`, return the default base URL
    /// (`http://localhost:11434/v1`) when no override is configured.
    pub const OLLAMA_DEFAULT_BASE_URL: &'static str = "http://localhost:11434/v1";

    /// Construct from a `DistillerSection` for back-compat resolution.
    pub fn from_distiller(d: &DistillerSection) -> Self {
        Self {
            enabled: d.enabled,
            provider: d.provider.clone(),
            model: d.model.clone(),
            api_key_env: d.api_key_env.clone(),
            base_url_env: d.base_url_env.clone(),
            region: d.region.clone(),
            region_env: d.region_env.clone(),
        }
    }
}

/// S5.1: storage / retrieval backend configuration.
///
/// Controls which `RetrievalBackend` implementation is used for memory
/// candidate generation. The broker (scoring, floors, rerank, compression)
/// is backend-agnostic and is NOT affected by this setting.
///
/// `#[serde(default)]` keeps every pre-S5 project.toml loading cleanly
/// (they get `backend = "flat"`, which is exactly today's FTS + usearch-ANN
/// behaviour).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSection {
    /// Which retrieval backend to use.
    ///
    /// | Value         | Behaviour                                             |
    /// |---------------|-------------------------------------------------------|
    /// | `"flat"`      | FTS + usearch HNSW ANN (current, default)             |
    /// | `"graph-lite"`| TODO (S5.2): graph-augmented FTS + ANN               |
    /// | `"graph"`     | TODO (future): full graph traversal                   |
    ///
    /// Unknown values fall back to `"flat"` with a warning.
    #[serde(default = "default_storage_backend")]
    pub backend: String,
}

fn default_storage_backend() -> String {
    "flat".to_string()
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            backend: default_storage_backend(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSection {
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub max_output_tokens: u32,
    pub temperature: f32,
    pub request_timeout_secs: u64,
    /// AWS Bedrock: literal region (e.g. `us-east-1`). Takes precedence over
    /// `region_env`. `#[serde(default)]` keeps existing project.toml loading.
    #[serde(default)]
    pub region: Option<String>,
    /// AWS Bedrock: env-var name that holds the region. Defaults to
    /// `"AWS_REGION"` via `default_region_env()`. Consulted only when
    /// `region` is `None`. `#[serde(default)]` keeps existing project.toml
    /// loading cleanly.
    #[serde(default = "default_region_env")]
    pub region_env: String,
    /// v1.5: override the built-in $/MTok price table for the ROI ledger.
    /// When set, this value is used for USD conversion instead of the
    /// approximate built-in table.  Useful for private-endpoint pricing or
    /// non-standard model deployments.  `#[serde(default)]` keeps all
    /// pre-v1.5 project.toml files loading cleanly (they get `None`).
    #[serde(default)]
    pub price_per_mtok: Option<f64>,
}

fn default_region_env() -> String {
    "AWS_REGION".to_string()
}

impl Default for ModelSection {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_string(),
            model: "claude-opus-4-7".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_output_tokens: 8192,
            temperature: 0.2,
            request_timeout_secs: 120,
            region: None,
            region_env: default_region_env(),
            price_per_mtok: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerSection {
    /// Flat per-stage budget (tokens). Used as a fallback when
    /// `task_size == 0` (broker disabled or task-size signal unavailable)
    /// and as the compat default for pre-F3 project.toml files.
    /// For live runs the adaptive budget (`adaptive_budget`) supersedes this.
    pub default_budget_tokens: u32,
    pub weights: BrokerWeights,
    /// D1f: hard cap on capsules rendered into a model prompt. The
    /// broker may surface more capsules than this (up to the token
    /// budget), but the pipeline render step truncates to this cap so
    /// a tighter, higher-precision capsule set isn't silently padded
    /// back to a larger number. 0 = disabled (budget-only limit).
    ///
    /// Default 8: lower than the old hard-coded 12 so precision from
    /// D1e wins; operators can raise it per-project in project.toml.
    /// `#[serde(default)]` keeps pre-D1f project.toml files loading.
    #[serde(default = "default_max_capsules")]
    pub max_capsules: usize,
    /// D1e: absolute minimum cosine similarity between the query
    /// embedding and a candidate embedding required for the candidate
    /// to survive budgeting. When > 0.0, candidates whose cosine is
    /// strictly below this threshold are dropped BEFORE the MMR pass
    /// so a genuinely-irrelevant corpus hits the zero-capsule skipped
    /// path more often. Inert on lean (NoopEmbedder) builds because
    /// there is no query embedding to compare against.
    ///
    /// Default -1.0 = AUTO (v1.0.0): the right floor is MODEL-DEPENDENT,
    /// because cosine scales differ per embedder. bge-family cosines for
    /// related pairs sit well above ~0.5 with noise below ~0.4, so auto
    /// resolves to 0.35 there. jina-v2 cosines run lower — the remote
    /// benchmark showed a 0.35 floor KILLING relevant results outright
    /// (MRR 0.90 → 0.77, recall@2 == recall@4) — and that model's own
    /// precision already keeps noise low (~1.2 vs bge's ~4.0 capsules on
    /// no-answer queries, floors off), so auto resolves to 0.0 (disabled)
    /// for non-bge models. Set an explicit value to override auto in
    /// either direction; 0.0 disables. `#[serde(default = …)]` keeps older
    /// configs loading with auto.
    #[serde(default = "default_min_semantic_score")]
    pub min_semantic_score: f32,
    /// v1.0.0: absolute *lexical* relevance floor for memory candidates,
    /// expressed as the fraction of the query's IDF-weighted discriminating
    /// power a memory must lexically cover to survive. Unlike
    /// `min_semantic_score` (which needs a query embedding and is therefore
    /// inert on the FTS-only `UserPromptSubmit` hook path), this floor works
    /// on lexical retrieval — closing the gap where a broad conceptual query
    /// ("what's the idea of the repo") surfaces unrelated memories that only
    /// share a corpus-ubiquitous token like the project name.
    ///
    /// Mechanics: query tokens are stripped of stopwords; each remaining
    /// token is IDF-weighted over the memory corpus (so the project name,
    /// present in nearly every memory, contributes ~0). A memory is dropped
    /// when the IDF-weighted share of the query it covers is below this floor
    /// AND it has no semantic support. Repo-file/manifest capsules pass
    /// through untouched (their FTS match on file content is itself the
    /// relevance signal, and overview queries *want* the README).
    ///
    /// Default 0.5 = "must cover the more-discriminating half of the query."
    /// 0.0 disables the floor. `#[serde(default = …)]` keeps older configs
    /// loading with the floor active.
    #[serde(default = "default_min_lexical_coverage")]
    pub min_lexical_coverage: f32,
    /// F3: floor for the adaptive per-stage brain budget. Small tasks
    /// receive at least this many tokens so the brain is never starved.
    /// `#[serde(default)]` keeps pre-F3 project.toml files loading cleanly.
    #[serde(default = "default_budget_floor_tokens")]
    pub budget_floor_tokens: u32,
    /// F3: per-run global ceiling on brain-injected tokens across ALL
    /// stages combined. Later stages receive only the remaining capacity
    /// once earlier stages have been charged via the `RunRecallLedger`.
    /// `#[serde(default)]` keeps pre-F3 project.toml files loading cleanly.
    #[serde(default = "default_budget_run_cap_tokens")]
    pub budget_run_cap_tokens: u32,
    /// W3.2: persistent ambient-context off-switch. When false, the
    /// workspace fingerprint (branch, recent files, dirty status) is not
    /// collected or appended to the retrieval query. Precedence:
    /// `KIMETSU_BRAIN_AMBIENT` env override > this field > default (true).
    /// `#[serde(default = "default_true")]` keeps pre-W3 project.toml
    /// files loading unchanged.
    #[serde(default = "default_true")]
    pub ambient: bool,
    /// v1.5 (Story 2.1): render-time capsule compression. When true (default),
    /// capsule summaries are compressed with [`compress_for_render`] before
    /// being injected into hook stdout or MCP tool responses. Compression
    /// strips `[tags: ...]` / `(context: ...)` annotations and caps at 3
    /// sentences. Ranking is NEVER affected — compression runs only after
    /// retrieval and reranking. Set false to inject full memory text (useful
    /// for debugging or when summaries are already concise).
    ///
    /// `#[serde(default = "default_true")]` keeps pre-v1.5 project.toml files
    /// loading cleanly (they get compression ON).
    #[serde(default = "default_true")]
    pub compress_capsules: bool,
    /// v1.5 (Story 2.3): session-scoped cross-turn capsule dedupe. When true
    /// (default), the `UserPromptSubmit` context hook skips capsules whose
    /// `expansion_handle` was already injected earlier in the same session
    /// (tracked via the proactive-state sidecar). A soft policy: skipping only
    /// happens when at least one NEW capsule remains — if dedupe would empty
    /// the injection entirely, all capsules are injected anyway (a repeated
    /// top memory may still be the right context). Set false to disable
    /// session dedupe and always inject the full ranked set.
    ///
    /// `#[serde(default = "default_true")]` keeps pre-v1.5 project.toml files
    /// loading cleanly (they get session dedupe ON).
    #[serde(default = "default_true")]
    pub session_dedupe: bool,
    /// Flagship 1 / Pass B: inject the repo digest + work-resume context at
    /// SessionStart. When true (default), `kimetsu brain session-start-hook`
    /// prints `additionalContext` JSON combining the ~400-token repo digest
    /// (1.1) and the episodic resume (Pass A).  Set false to suppress the
    /// warm-start injection entirely — useful when the host already provides
    /// repo context or the digest is not yet built.
    ///
    /// `#[serde(default = "default_true")]` keeps pre-Flagship-1 project.toml
    /// files loading cleanly (they get warm_start ON — the feature is additive
    /// and defaults to enabled so fresh installs get it immediately).
    #[serde(default = "default_true")]
    pub warm_start: bool,
    /// Flagship 3 / Pass B (3.3): minimum composite broker score for a capsule
    /// to receive the "Verified answer from project memory:" prefix at render
    /// time. This prefix signals to the model that it can act in one turn
    /// rather than re-verifying the information.
    ///
    /// STRICTLY ADDITIVE: only changes the rendered prefix of an already-top
    /// capsule. Ranking, floors, and capsule selection are NEVER affected.
    ///
    /// The threshold is deliberately conservative (0.92 default) so the marker
    /// is rare and only fires on genuinely unambiguous matches. Tune with
    /// `kimetsu brain bench` data (Epic S2) before lowering. Set to 1.1 (above
    /// the maximum achievable score) to disable entirely, or 0.0 to always
    /// mark any top capsule (not recommended — wait for regret data first).
    ///
    /// Regret guard: if the capsule's memory was recently dropped by floors in
    /// another retrieval context (appears in the dropped sidecar), the prefix
    /// is suppressed regardless of this threshold, preventing overconfident
    /// labelling of inconsistently-scored memories.
    ///
    /// `#[serde(default = …)]` keeps all pre-F3 project.toml files loading
    /// unchanged (they get the conservative default).
    #[serde(default = "default_answer_grade_min_score")]
    pub answer_grade_min_score: f32,
    /// Flagship 3 / Pass B (3.5): opt-in proactive pre-fetch at PreToolUse.
    ///
    /// When true, the PreToolUse hook does a LIGHTWEIGHT relevance warm based
    /// on the current tool's file path (in addition to the command text),
    /// surfacing a relevant memory before the agent edits or reads a file.
    /// The existing floors (min_score, max_capsules, session dedupe, refractory
    /// throttle) all apply — this is additive only.
    ///
    /// Default false (OFF): the PreToolUse hook behaviour is identical to
    /// before this flag existed. Graduating to default-on waits for regret
    /// data (Epic S2) to confirm that file-path-augmented queries don't
    /// increase noise. Enable per-project in project.toml once comfortable.
    ///
    /// `#[serde(default)]` keeps all pre-F3 project.toml files loading with
    /// the feature OFF (zero behaviour change for existing users).
    #[serde(default)]
    pub proactive_prefetch: bool,
}

fn default_max_capsules() -> usize {
    8
}

fn default_min_semantic_score() -> f32 {
    -1.0
}

fn default_min_lexical_coverage() -> f32 {
    0.5
}

fn default_budget_floor_tokens() -> u32 {
    1500
}

fn default_budget_run_cap_tokens() -> u32 {
    8000
}

/// F3 / Pass B (3.3): conservative answer-grade threshold. At 0.92 the marker
/// fires only when the retrieval pipeline (embedder + reranker) places the top
/// capsule in the very top of its score range — roughly 1-in-10 retrievals on
/// a well-populated brain. Lowering requires regret data from Epic S2 to
/// confirm precision stays high.
fn default_answer_grade_min_score() -> f32 {
    0.92
}

impl Default for BrokerSection {
    fn default() -> Self {
        Self {
            default_budget_tokens: 6000,
            weights: BrokerWeights::default(),
            max_capsules: default_max_capsules(),
            min_semantic_score: default_min_semantic_score(),
            min_lexical_coverage: default_min_lexical_coverage(),
            budget_floor_tokens: default_budget_floor_tokens(),
            budget_run_cap_tokens: default_budget_run_cap_tokens(),
            ambient: default_true(),
            compress_capsules: default_true(),
            session_dedupe: default_true(),
            warm_start: default_true(),
            answer_grade_min_score: default_answer_grade_min_score(),
            proactive_prefetch: false,
        }
    }
}

/// F3: compute the adaptive per-stage brain budget given a task-size signal.
///
/// **Task-size signal** (defined): `task_size = estimate_tokens(task_text) +
/// estimate_tokens(localized_file_context)`, where `estimate_tokens` uses the
/// same heuristic as the rest of the pipeline: `(whitespace_words * 1.33).ceil()`.
/// Localized-file context is the rendered list of paths surfaced before the
/// first implementation attempt.
///
/// **Scaling**: `floor + k * sqrt(task_size)` clamped to `[floor, run_cap]`.
/// sqrt is chosen because it grows slower than linear — doubling task_size
/// grows the budget by only ~41%, and a 5× task grows it by only ~124%
/// (well under 2×).
///
/// **Constant k**: chosen so a "typical" task (task_size ≈ 200 tokens, e.g.
/// a concise one-paragraph task + a handful of file paths) lands near
/// today's default 6000 tokens, avoiding a behavior cliff on upgrade.
///   k = (6000 - 1500) / sqrt(200) ≈ 318.2
///
/// **Fallback**: when `task_size == 0` (broker disabled, size signal
/// unavailable, or called pre-retrieval) returns `floor` — callers should
/// use `default_budget_tokens` instead in those paths.
///
/// **Per-run cap**: the caller is responsible for computing
/// `remaining = run_cap.saturating_sub(ledger.injected_tokens())` and passing
/// `min(adaptive_budget(...), remaining)` as the stage's `budget_tokens`.
pub fn adaptive_budget(task_size: u32, floor: u32, run_cap: u32) -> u32 {
    if task_size == 0 {
        return floor;
    }
    // k ≈ 318.2 so that adaptive_budget(200, 1500, 8000) ≈ 6000.
    // We scale k by 10 and work in integer arithmetic to avoid f64 in hot path.
    const K_SCALED: u32 = 3182; // k * 10
    let sqrt_part = (task_size as f64).sqrt();
    let budget_f = floor as f64 + (K_SCALED as f64 / 10.0) * sqrt_part;
    let budget = budget_f.round() as u32;
    budget.clamp(floor, run_cap)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerWeights {
    pub relevance: f32,
    pub confidence: f32,
    pub freshness: f32,
    pub scope: f32,
    pub localization: Option<StageWeights>,
    pub patch_plan: Option<StageWeights>,
    pub verification: Option<StageWeights>,
    pub review: Option<StageWeights>,
    /// v0.5.1: half-life (in days) for the usefulness-decay
    /// multiplier. A memory's effective usefulness contribution
    /// decays as `exp(-ln(2) * age_days / half_life)` where age is
    /// measured from `last_useful_at` if present, else
    /// `created_at`. 30 days = a 6-month-old useful memory ends
    /// up at ~1.5% of its original weight; tune lower for faster-
    /// changing repos, higher for slow-evolving ones.
    ///
    /// `#[serde(default)]` keeps pre-v0.5.1 project.toml files
    /// loading cleanly — they get the 30-day default.
    #[serde(default = "default_decay_half_life_days")]
    pub decay_half_life_days: f32,
}

fn default_decay_half_life_days() -> f32 {
    30.0
}

impl Default for BrokerWeights {
    fn default() -> Self {
        Self {
            relevance: 0.50,
            confidence: 0.20,
            freshness: 0.20,
            scope: 0.10,
            localization: Some(StageWeights {
                relevance: 0.70,
                confidence: 0.10,
                freshness: 0.10,
                scope: 0.10,
            }),
            patch_plan: Some(StageWeights {
                relevance: 0.40,
                confidence: 0.30,
                freshness: 0.10,
                scope: 0.20,
            }),
            verification: Some(StageWeights {
                relevance: 0.40,
                confidence: 0.10,
                freshness: 0.40,
                scope: 0.10,
            }),
            review: Some(StageWeights {
                relevance: 0.50,
                confidence: 0.20,
                freshness: 0.20,
                scope: 0.10,
            }),
            decay_half_life_days: default_decay_half_life_days(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageWeights {
    pub relevance: f32,
    pub confidence: f32,
    pub freshness: f32,
    pub scope: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellSection {
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub env_allowlist_extra: Vec<String>,
    pub redact_secrets: bool,
}

impl Default for ShellSection {
    fn default() -> Self {
        Self {
            default_timeout_secs: 60,
            max_timeout_secs: 600,
            env_allowlist_extra: vec!["RUSTFLAGS".to_string(), "CARGO_HOME".to_string()],
            redact_secrets: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionSection {
    pub max_file_bytes: u64,
    pub extra_skip_dirs: Vec<String>,
    pub max_total_files: u64,
    /// v1.0: enable/disable the per-add conflict-detection scan.
    ///
    /// Default true. Set to false (or set env `KIMETSU_DETECT_CONFLICTS=0`)
    /// to skip the cosine-similarity conflict scan at add time — useful when
    /// bulk-seeding a brain where the O(N²) scan would be prohibitively slow.
    /// Review `kimetsu brain memory conflicts` afterwards to catch any
    /// contradictions.
    ///
    /// Precedence: `KIMETSU_DETECT_CONFLICTS` env > this field > default.
    #[serde(default = "default_true")]
    pub detect_conflicts: bool,
    /// v2.5 Pass B (Story 1.3): enable automatic contradiction resolution.
    ///
    /// When true (default), conflicting memory pairs are scored by
    /// `confidence × recency`.  Clear winners (score gap ≥ 0.15) have the
    /// loser's `valid_to` stamped to now via `mark_memory_temporal`
    /// (event-sourced, rebuild-safe).  Near-ties are queued in
    /// `memory_conflicts` for operator review, same as the v0.5.2 behavior.
    ///
    /// Set to false (or set env `KIMETSU_RESOLVE_CONFLICTS=0`) to revert to
    /// detect-only mode: all conflicts are queued for the operator.
    ///
    /// Precedence: `KIMETSU_RESOLVE_CONFLICTS` env > this field > default.
    /// Resolution only runs when `detect_conflicts` is also enabled.
    #[serde(default = "default_true")]
    pub resolve_conflicts: bool,

    /// Flagship 2 / Story 2.1: seed a non-zero initial usefulness_score for
    /// new memories at write time.
    ///
    /// Uses a rule-based estimator (kind weight + rarity bonus) — no model
    /// required.  The value is stored in the `memory.accepted` event payload
    /// (`initial_usefulness`) and applied by the projector (rebuild-safe).
    /// Set false to keep the v0 default of 0.0 for all new memories.
    /// `#[serde(default = "default_true")]` keeps pre-Flagship-2 project.toml
    /// files loading cleanly (they get the feature ON).
    #[serde(default = "default_true")]
    pub initial_importance_scoring: bool,

    /// Flagship 2 / Story 2.2: quality-control filter in the distiller.
    /// Drop lessons that are near-duplicates (cosine ≥ threshold), too long,
    /// too short, or contain transience markers.  Default true.
    /// `#[serde(default = "default_true")]` keeps older configs loading cleanly.
    #[serde(default = "default_true")]
    pub quality_filter_enabled: bool,

    /// Flagship 2 / Story 2.2: novelty threshold — cosine ≥ this value → DROP.
    /// Default 0.9.  `#[serde(default)]` keeps older configs loading cleanly
    /// (they get the default via the `Default` impl).
    #[serde(default = "default_quality_filter_novelty_threshold")]
    pub quality_filter_novelty_threshold: f32,

    /// Flagship 2 / Story 2.2: minimum lesson length in chars (after trim).
    /// Lessons shorter than this are dropped.  Default 10.
    #[serde(default = "default_quality_filter_min_len")]
    pub quality_filter_min_len: usize,

    /// Flagship 2 / Story 2.2: maximum lesson length in chars (after trim).
    /// Lessons longer than this are dropped.  Default 500.
    #[serde(default = "default_quality_filter_max_len")]
    pub quality_filter_max_len: usize,
}

fn default_quality_filter_novelty_threshold() -> f32 {
    0.9
}
fn default_quality_filter_min_len() -> usize {
    10
}
fn default_quality_filter_max_len() -> usize {
    500
}

impl Default for IngestionSection {
    fn default() -> Self {
        Self {
            max_file_bytes: 524_288,
            extra_skip_dirs: Vec::new(),
            max_total_files: 50_000,
            detect_conflicts: true,
            resolve_conflicts: true,
            initial_importance_scoring: true,
            quality_filter_enabled: true,
            quality_filter_novelty_threshold: default_quality_filter_novelty_threshold(),
            quality_filter_min_len: default_quality_filter_min_len(),
            quality_filter_max_len: default_quality_filter_max_len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSection {
    pub max_total_tool_calls: u32,
    pub max_total_model_turns: u32,
    pub max_total_cost_usd: f32,
}

impl Default for RunSection {
    fn default() -> Self {
        // `max_total_cost_usd` is treated as advisory under subscription-based
        // providers (e.g. Claude Code OAuth). The agent loop still enforces it
        // when it does fire, but the default is set high enough that it
        // functions as a runaway-prevention safety net rather than a per-run
        // budget. Tighten in `project.toml` when running against a metered
        // provider.
        Self {
            max_total_tool_calls: 60,
            max_total_model_turns: 30,
            max_total_cost_usd: 250.0,
        }
    }
}

/// Epic S3: personal brain sync configuration.
///
/// Controls the event-log replication directory protocol.  When `dir` is
/// absent, the sync subcommand is unconfigured and prints a setup hint.
///
/// `machine_id` is a stable opaque identifier for this machine.  It defaults
/// to a freshly-generated ULID that is persisted in project.toml on first
/// use (written by `kimetsu brain sync --setup`).  Operators can set it
/// manually to a meaningful name (hostname, username, etc.) — just keep it
/// unique within the sync directory.
///
/// `#[serde(default)]` keeps every pre-S3 project.toml loading cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncSection {
    /// Absolute (or home-relative) path to the shared sync directory.
    /// Each machine writes its batches under `<dir>/<machine_id>/`.
    /// When `None`, syncing is unconfigured.
    #[serde(default)]
    pub dir: Option<String>,
    /// Stable machine identifier.  Defaults to an empty string (= not yet
    /// set; the CLI generates one on first use).
    #[serde(default)]
    pub machine_id: String,
}

// ---------------------------------------------------------------------------
// F3 Flagship 3 / Lifecycle & forgetting configuration
// ---------------------------------------------------------------------------

/// F3 lifecycle / forgetting policy configuration.
///
/// All settings are gated behind `forget_enabled = false` by default so
/// existing installs are completely unaffected until an operator opts in.
///
/// `#[serde(default)]` keeps all existing project.toml files loading cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleSection {
    // ---- Story 3.1: Active forgetting ----
    /// Master opt-in switch. Default false — forgetting is NEVER triggered
    /// without the operator explicitly enabling it.
    #[serde(default)]
    pub forget_enabled: bool,

    /// Minimum age (days) before a memory is eligible for archival.
    /// Only memories whose `last_useful_at` (or `created_at` when never cited)
    /// is older than this many days are considered. Default 90.
    #[serde(default = "default_forget_min_age_days")]
    pub forget_min_age_days: u32,

    /// Usefulness floor: memories with `usefulness_score / max(use_count, 1)
    /// <= this` value are candidates. Default -0.1 (net-negative).
    #[serde(default = "default_forget_usefulness_floor")]
    pub forget_usefulness_floor: f32,

    /// Evergreen protection threshold. Memories with
    /// `use_count >= forget_protect_use_count` are NEVER archived regardless
    /// of their usefulness ratio. Default 10.
    #[serde(default = "default_forget_protect_use_count")]
    pub forget_protect_use_count: u32,

    // ---- Story 3.2: Regret-driven review ----
    /// Number of `retrieval.regret` events a memory must accumulate before
    /// it appears in the review list. Default 5.
    #[serde(default = "default_regret_flag_threshold")]
    pub regret_flag_threshold: u64,

    // ---- Story 3.3: Proposal-queue hygiene ----
    /// Number of days before a pending proposal is auto-expired (rejected with
    /// reason "expired"). Default 30. 0 disables expiry.
    #[serde(default = "default_proposal_expiry_days")]
    pub proposal_expiry_days: u32,

    /// Proposals with `proposed_confidence >= this` value are auto-accepted
    /// during the hygiene pass. Default 1.1 (disabled — threshold above the
    /// maximum possible confidence of 1.0).
    #[serde(default = "default_proposal_auto_accept_confidence")]
    pub proposal_auto_accept_confidence: f32,
}

fn default_forget_min_age_days() -> u32 {
    90
}
fn default_forget_usefulness_floor() -> f32 {
    -0.1
}
fn default_forget_protect_use_count() -> u32 {
    10
}
fn default_regret_flag_threshold() -> u64 {
    5
}
fn default_proposal_expiry_days() -> u32 {
    30
}
fn default_proposal_auto_accept_confidence() -> f32 {
    1.1 // disabled: above max confidence
}

impl Default for LifecycleSection {
    fn default() -> Self {
        Self {
            forget_enabled: false,
            forget_min_age_days: default_forget_min_age_days(),
            forget_usefulness_floor: default_forget_usefulness_floor(),
            forget_protect_use_count: default_forget_protect_use_count(),
            regret_flag_threshold: default_regret_flag_threshold(),
            proposal_expiry_days: default_proposal_expiry_days(),
            proposal_auto_accept_confidence: default_proposal_auto_accept_confidence(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-v0.8 project.toml has no `[embedder]` table. The
    /// `#[serde(default)]` on the field must keep it loading cleanly,
    /// defaulting to the lean English model.
    #[test]
    fn pre_v0_8_config_without_embedder_loads_with_default() {
        let toml = r#"
[kimetsu]
project_id = "demo"
schema_version = 7

[model]
provider = "anthropic"
model = "claude-opus-4-7"
api_key_env = "ANTHROPIC_API_KEY"
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

[broker.weights]
relevance = 0.5
confidence = 0.2
freshness = 0.2
scope = 0.1

[shell]
default_timeout_secs = 60
max_timeout_secs = 600
env_allowlist_extra = []
redact_secrets = true

[ingestion]
max_file_bytes = 524288
extra_skip_dirs = []
max_total_files = 50000

[run]
max_total_tool_calls = 60
max_total_model_turns = 30
max_total_cost_usd = 250.0
"#;
        let config = ProjectConfig::from_toml(toml).expect("pre-v0.8 toml must load");
        assert_eq!(config.embedder.model, "jina-v2-base-code");
        // A pre-v0.8.5 toml has no [learning] section — auto-harvest
        // defaults on so existing installs gain the behavior on upgrade.
        assert!(config.learning.auto_harvest);
        // A pre-distiller toml has no [learning.distiller] — defaults to off,
        // anthropic, claude-haiku-4-5.
        assert!(!config.learning.distiller.enabled);
        assert_eq!(config.learning.distiller.provider, "anthropic");
        assert_eq!(config.learning.distiller.model, "claude-haiku-4-5");
        assert_eq!(config.learning.distiller.api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(config.learning.distiller.base_url_env, "ANTHROPIC_BASE_URL");
        // D1e/D1f: pre-D1 configs without max_capsules / min_semantic_score
        // must load cleanly and receive the safe defaults.
        assert_eq!(config.broker.max_capsules, 8);
        // v1.0.0: the semantic floor is ON by default (was 0.0/disabled) now
        // that the warm daemon serves semantic retrieval to every prompt.
        assert_eq!(config.broker.min_semantic_score, -1.0, "auto sentinel");
        // v1.0.0: a config without min_lexical_coverage loads with the floor
        // active at its default (0.5), so existing installs gain the relevance
        // gate on upgrade.
        assert_eq!(config.broker.min_lexical_coverage, 0.5);
        // F3: pre-F3 configs without budget_floor_tokens / budget_run_cap_tokens
        // must load cleanly and receive the safe defaults.
        assert_eq!(config.broker.budget_floor_tokens, 1500);
        assert_eq!(config.broker.budget_run_cap_tokens, 8000);
        // W3: pre-W3 configs without the new off-switch fields must load
        // cleanly and default to enabled (true) for all three features.
        assert!(
            config.embedder.enabled,
            "W3.1: embedder.enabled must default to true"
        );
        assert!(
            config.broker.ambient,
            "W3.2: broker.ambient must default to true"
        );
        assert!(
            config.kimetsu.use_user_brain,
            "W3.3: kimetsu.use_user_brain must default to true"
        );
        // v1.0.0: daemon + warm_on_start default ON so existing installs get
        // the warm-daemon path on upgrade.
        assert!(
            config.embedder.daemon,
            "embedder.daemon must default to true"
        );
        assert!(
            config.embedder.warm_on_start,
            "embedder.warm_on_start must default to true"
        );
        // v1.0.0: reranker defaults to jina-reranker-v1-turbo-en so existing
        // v1.0.0: a config without mcp_write_tools loads with local write
        // tools ENABLED, so the record-a-lesson workflow the brain itself
        // prescribes works out of the box on upgrade.
        assert!(
            config.kimetsu.mcp_write_tools,
            "kimetsu.mcp_write_tools must default to true"
        );
        // v1.0.0: jina-tiny + pool 6 measured as fitting the hook's 300ms
        // budget on real memories with the best benchmark quality, so the
        // reranker is ON by default.
        assert_eq!(
            config.embedder.reranker, "ms-marco-tinybert-l-2-v2",
            "embedder.reranker must default to ms-marco-tinybert-l-2-v2"
        );
        // v1.5: a pre-v1.5 project.toml has no learning.store_queries —
        // defaults to true so existing installs gain the eval-set signal
        // on upgrade without any config change.
        assert!(
            config.learning.store_queries,
            "learning.store_queries must default to true"
        );
        // v1.5 (Story 2.1): a pre-v1.5 project.toml without broker.compress_capsules
        // must load cleanly and default to true (compression ON).
        assert!(
            config.broker.compress_capsules,
            "broker.compress_capsules must default to true"
        );
        // v1.5 (Story 2.3): a pre-v1.5 project.toml without broker.session_dedupe
        // must load cleanly and default to true (dedupe ON).
        assert!(
            config.broker.session_dedupe,
            "broker.session_dedupe must default to true"
        );
        // Flagship 1 Pass B: a pre-Flagship-1 project.toml without
        // broker.warm_start must load cleanly and default to true (warm-start ON).
        assert!(
            config.broker.warm_start,
            "broker.warm_start must default to true"
        );
        // S5.1: a pre-S5 project.toml without [storage] must load cleanly
        // and default to backend = "flat" (the existing FTS + ANN path).
        assert_eq!(
            config.storage.backend, "flat",
            "storage.backend must default to \"flat\" when absent"
        );
        // F3 Pass B (3.3): a pre-F3 project.toml without broker.answer_grade_min_score
        // must load cleanly and receive the conservative default (0.92).
        assert!(
            (config.broker.answer_grade_min_score - 0.92).abs() < f32::EPSILON,
            "broker.answer_grade_min_score must default to 0.92"
        );
        // F3 Pass B (3.5): a pre-F3 project.toml without broker.proactive_prefetch
        // must load cleanly and default to false (opt-in, OFF by default).
        assert!(
            !config.broker.proactive_prefetch,
            "broker.proactive_prefetch must default to false (opt-in)"
        );
        // S3: a pre-S3 project.toml without [sync] must load cleanly and
        // default to no sync dir and empty machine_id.
        assert!(
            config.sync.dir.is_none(),
            "sync.dir must default to None when absent"
        );
        assert!(
            config.sync.machine_id.is_empty(),
            "sync.machine_id must default to empty string when absent"
        );
    }

    /// A1: default_for_project must use KIMETSU_CONFIG_VERSION (the
    /// project.toml format version), NOT KIMETSU_SCHEMA_VERSION (the brain.db
    /// schema). The two constants are intentionally decoupled so a DB-schema
    /// bump does not force every project.toml to be rewritten.
    #[test]
    fn default_config_uses_config_version_not_schema_version() {
        let cfg = ProjectConfig::default_for_project("p1");
        assert_eq!(cfg.kimetsu.schema_version, crate::KIMETSU_CONFIG_VERSION);
    }

    /// `model set` writes the whole config back via `to_toml`; a
    /// round-trip must preserve the chosen embedder (and other sections).
    #[test]
    fn embedder_survives_toml_round_trip() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.embedder.model = "bge-m3".to_string();
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert_eq!(reloaded.embedder.model, "bge-m3");
        assert_eq!(reloaded.broker.default_budget_tokens, 6000);
        assert_eq!(reloaded.kimetsu.project_id, "demo");
        // F3 fields survive round-trip.
        assert_eq!(reloaded.broker.budget_floor_tokens, 1500);
        assert_eq!(reloaded.broker.budget_run_cap_tokens, 8000);
        // W3 off-switch fields survive round-trip.
        assert!(reloaded.embedder.enabled);
        assert!(reloaded.broker.ambient);
        assert!(reloaded.kimetsu.use_user_brain);
    }

    /// W3: the three new off-switch fields can be set to `false` in
    /// project.toml and round-trip cleanly.
    #[test]
    fn w3_off_switch_fields_round_trip_as_false() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.embedder.enabled = false;
        config.broker.ambient = false;
        config.kimetsu.use_user_brain = false;
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert!(
            !reloaded.embedder.enabled,
            "embedder.enabled must survive as false"
        );
        assert!(
            !reloaded.broker.ambient,
            "broker.ambient must survive as false"
        );
        assert!(
            !reloaded.kimetsu.use_user_brain,
            "kimetsu.use_user_brain must survive as false"
        );
        // Unrelated fields unaffected.
        assert_eq!(reloaded.kimetsu.project_id, "demo");
    }

    // ── F3: adaptive_budget unit tests ────────────────────────────────────

    /// F3-budget-1: floor is returned when task_size == 0.
    #[test]
    fn f3_adaptive_budget_zero_size_returns_floor() {
        assert_eq!(
            super::adaptive_budget(0, 1500, 8000),
            1500,
            "task_size=0 must return floor"
        );
    }

    /// F3-budget-2: run_cap is returned when task_size is enormous (very large).
    #[test]
    fn f3_adaptive_budget_huge_size_clamped_to_run_cap() {
        let result = super::adaptive_budget(1_000_000, 1500, 8000);
        assert_eq!(result, 8000, "huge task_size must be clamped to run_cap");
    }

    /// F3-budget-3: budget grows SUBLINEARLY — adaptive_budget(5*T) < 2 * adaptive_budget(T).
    ///
    /// With sqrt scaling: budget(5*T) / budget(T) = (floor + k*sqrt(5T)) / (floor + k*sqrt(T))
    /// < sqrt(5) ≈ 2.236 for large T, but also < 2 for T in the practical range
    /// because the floor term dominates at small sizes and sqrt(5) dominates at large
    /// sizes. Specifically for T=200: budget(200)≈6000, budget(1000)≈8000 (capped) → ratio < 2.
    /// For T=50 (below cap): budget(50)≈3751, budget(250)≈6534 → ratio ≈ 1.74 < 2. ✓
    #[test]
    fn f3_adaptive_budget_is_sublinear() {
        let floor = 1500u32;
        let run_cap = 16_000u32; // raised cap for this test so neither hits the ceiling

        // T0 = 200 tokens (concise task), 5*T0 = 1000 tokens (verbose task)
        let t0 = 200u32;
        let b_t0 = super::adaptive_budget(t0, floor, run_cap);
        let b_5t0 = super::adaptive_budget(5 * t0, floor, run_cap);

        assert!(
            b_5t0 < 2 * b_t0,
            "sublinear guarantee: adaptive_budget(5*T)={b_5t0} must be < 2*adaptive_budget(T)={} (T={t0})",
            2 * b_t0
        );
        assert!(
            b_5t0 > b_t0,
            "budget must still grow: adaptive_budget(5*T)={b_5t0} > adaptive_budget(T)={b_t0}"
        );
    }

    /// F3-budget-4: a typical task (task_size ≈ 200) lands near the historical
    /// default of 6000 tokens, avoiding a behavior cliff on upgrade.
    #[test]
    fn f3_adaptive_budget_typical_task_near_historical_default() {
        let budget = super::adaptive_budget(200, 1500, 8000);
        // k = 318.2 → floor + k*sqrt(200) = 1500 + 318.2*14.14 ≈ 5999
        // Allow ±300 to tolerate rounding.
        assert!(
            (5700..=8000).contains(&budget),
            "typical task budget expected near 6000, got {budget}"
        );
    }

    /// F3-budget-5: floor is always respected — even small tasks get at least floor.
    #[test]
    fn f3_adaptive_budget_respects_floor() {
        for size in [1u32, 5, 10, 50] {
            let b = super::adaptive_budget(size, 1500, 8000);
            assert!(
                b >= 1500,
                "task_size={size}: budget={b} must be >= floor=1500"
            );
        }
    }

    /// F3-budget-6: run_cap is always respected — large tasks never exceed cap.
    #[test]
    fn f3_adaptive_budget_respects_run_cap() {
        for size in [500u32, 1000, 5000, 100_000] {
            let b = super::adaptive_budget(size, 1500, 8000);
            assert!(
                b <= 8000,
                "task_size={size}: budget={b} must be <= run_cap=8000"
            );
        }
    }

    /// v1.5: a pre-v1.5 project.toml without `model.price_per_mtok` must
    /// load cleanly and default to `None` (backward compatibility).
    #[test]
    fn pre_v1_5_config_without_price_per_mtok_loads_with_none() {
        let toml = r#"
[kimetsu]
project_id = "demo"
schema_version = 7

[model]
provider = "anthropic"
model = "claude-sonnet-4-7"
api_key_env = "ANTHROPIC_API_KEY"
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

[broker.weights]
relevance = 0.5
confidence = 0.2
freshness = 0.2
scope = 0.1

[shell]
default_timeout_secs = 60
max_timeout_secs = 600
env_allowlist_extra = []
redact_secrets = true

[ingestion]
max_file_bytes = 524288
extra_skip_dirs = []
max_total_files = 50000

[run]
max_total_tool_calls = 60
max_total_model_turns = 30
max_total_cost_usd = 250.0
"#;
        let config = ProjectConfig::from_toml(toml).expect("pre-v1.5 toml must load");
        assert!(
            config.model.price_per_mtok.is_none(),
            "price_per_mtok must default to None when absent from project.toml"
        );
    }

    /// v1.5 (Story 2.1+2.3): compress_capsules and session_dedupe survive a
    /// round-trip through serialize → deserialize when set to false.
    #[test]
    fn broker_v1_5_fields_round_trip_as_false() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.broker.compress_capsules = false;
        config.broker.session_dedupe = false;
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert!(
            !reloaded.broker.compress_capsules,
            "compress_capsules must survive as false"
        );
        assert!(
            !reloaded.broker.session_dedupe,
            "session_dedupe must survive as false"
        );
    }

    /// v1.5: when `model.price_per_mtok` is set in project.toml it must
    /// round-trip cleanly through serialize → deserialize.
    #[test]
    fn price_per_mtok_round_trips() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.model.price_per_mtok = Some(7.5);
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert_eq!(
            reloaded.model.price_per_mtok,
            Some(7.5),
            "price_per_mtok must round-trip"
        );
    }

    // ── S1.2: cheap_model() resolver tests ───────────────────────────────

    /// S1.2-a: a toml with only `[learning.distiller]` (no `[cheap_model]`)
    /// resolves via back-compat and returns the distiller's settings.
    #[test]
    fn s1_2_a_learning_distiller_back_compat() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.learning.distiller.enabled = true;
        config.learning.distiller.provider = "openai".to_string();
        config.learning.distiller.model = "gpt-5.4-mini".to_string();
        config.cheap_model = None;

        let resolved = config.cheap_model().expect("back-compat must resolve");
        assert_eq!(resolved.provider, "openai");
        assert_eq!(resolved.model, "gpt-5.4-mini");
        assert!(resolved.enabled);
    }

    /// S1.2-b: `[cheap_model]` takes precedence over `[learning.distiller]`
    /// when both are present and enabled.
    #[test]
    fn s1_2_b_cheap_model_takes_precedence() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.learning.distiller.enabled = true;
        config.learning.distiller.provider = "anthropic".to_string();
        config.learning.distiller.model = "claude-haiku-4-5".to_string();
        config.cheap_model = Some(super::CheapModelSection {
            enabled: true,
            provider: "ollama".to_string(),
            model: "qwen2.5:3b".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url_env: "OLLAMA_BASE_URL".to_string(),
            region: None,
            region_env: "AWS_REGION".to_string(),
        });

        let resolved = config.cheap_model().expect("cheap_model must resolve");
        assert_eq!(
            resolved.provider, "ollama",
            "[cheap_model] must win over [learning.distiller]"
        );
        assert_eq!(resolved.model, "qwen2.5:3b");
    }

    /// S1.2-c: provider=ollama round-trips and the OLLAMA_DEFAULT_BASE_URL
    /// constant has the expected value.
    #[test]
    fn s1_2_c_ollama_default_base_url() {
        assert_eq!(
            super::CheapModelSection::OLLAMA_DEFAULT_BASE_URL,
            "http://localhost:11434/v1",
            "ollama default base URL must point to localhost:11434/v1"
        );

        let mut config = ProjectConfig::default_for_project("demo");
        config.cheap_model = Some(super::CheapModelSection {
            enabled: true,
            provider: "ollama".to_string(),
            model: "llama3.2:3b".to_string(),
            api_key_env: "OLLAMA_API_KEY".to_string(),
            base_url_env: "OLLAMA_BASE_URL".to_string(),
            region: None,
            region_env: "AWS_REGION".to_string(),
        });

        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        let cm = reloaded.cheap_model().expect("ollama section must resolve");
        assert_eq!(cm.provider, "ollama");
        assert_eq!(cm.model, "llama3.2:3b");
    }

    /// S1.2-d: absent/disabled cheap model → resolver returns None;
    /// consumers that call `.cheap_model()` degrade gracefully (no panic).
    #[test]
    fn s1_2_d_absent_disabled_returns_none() {
        // (i) Neither section present/enabled.
        let config = ProjectConfig::default_for_project("demo");
        assert!(
            config.cheap_model().is_none(),
            "no cheap model configured: must return None"
        );

        // (ii) [cheap_model] present but disabled.
        let mut config2 = ProjectConfig::default_for_project("demo");
        config2.cheap_model = Some(super::CheapModelSection {
            enabled: false,
            ..super::CheapModelSection::default()
        });
        assert!(
            config2.cheap_model().is_none(),
            "disabled cheap_model must return None"
        );

        // (iii) [learning.distiller] present but disabled → back-compat returns None.
        let mut config3 = ProjectConfig::default_for_project("demo");
        config3.learning.distiller.enabled = false;
        assert!(
            config3.cheap_model().is_none(),
            "disabled learning.distiller must return None via back-compat"
        );
    }

    /// S1.2: a pre-S1.2 project.toml (no `[cheap_model]` section) must load
    /// cleanly with `cheap_model = None`.
    #[test]
    fn pre_s1_2_config_without_cheap_model_loads_cleanly() {
        let toml = r#"
[kimetsu]
project_id = "demo"
schema_version = 7

[model]
provider = "anthropic"
model = "claude-opus-4-7"
api_key_env = "ANTHROPIC_API_KEY"
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

[broker.weights]
relevance = 0.5
confidence = 0.2
freshness = 0.2
scope = 0.1

[shell]
default_timeout_secs = 60
max_timeout_secs = 600
env_allowlist_extra = []
redact_secrets = true

[ingestion]
max_file_bytes = 524288
extra_skip_dirs = []
max_total_files = 50000

[run]
max_total_tool_calls = 60
max_total_model_turns = 30
max_total_cost_usd = 250.0
"#;
        let config = ProjectConfig::from_toml(toml).expect("pre-S1.2 toml must load");
        assert!(
            config.cheap_model.is_none(),
            "cheap_model field must be None when absent from project.toml"
        );
        // And the resolver must return None (no distiller enabled either).
        assert!(
            config.cheap_model().is_none(),
            "cheap_model() must return None when no cheap model is configured"
        );
    }

    // ── S5.1: StorageSection tests ────────────────────────────────────────

    /// S5.1-a: `storage.backend` survives a round-trip through
    /// serialize → deserialize for each known variant string.
    #[test]
    fn s5_1_storage_backend_round_trips() {
        for variant in &["flat", "graph-lite", "graph"] {
            let mut config = ProjectConfig::default_for_project("demo");
            config.storage.backend = (*variant).to_string();
            let serialized = config.to_toml().expect("serialize");
            let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
            assert_eq!(
                reloaded.storage.backend, *variant,
                "storage.backend=\"{}\" must round-trip",
                variant
            );
        }
    }

    /// S5.1-b: `default_for_project` uses `backend = "flat"`.
    #[test]
    fn s5_1_default_for_project_uses_flat_backend() {
        let config = ProjectConfig::default_for_project("demo");
        assert_eq!(
            config.storage.backend, "flat",
            "default project config must use flat backend"
        );
    }

    // ── F3 Pass B: answer_grade_min_score + proactive_prefetch tests ─────────

    /// F3-B-1: new fields survive a round-trip through serialize → deserialize
    /// with non-default values.
    #[test]
    fn f3b_new_broker_fields_round_trip() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.broker.answer_grade_min_score = 0.85;
        config.broker.proactive_prefetch = true;
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert!(
            (reloaded.broker.answer_grade_min_score - 0.85).abs() < f32::EPSILON,
            "answer_grade_min_score must round-trip"
        );
        assert!(
            reloaded.broker.proactive_prefetch,
            "proactive_prefetch must round-trip as true"
        );
    }

    /// F3-B-2: proactive_prefetch = false (default) survives round-trip.
    #[test]
    fn f3b_proactive_prefetch_default_false_round_trips() {
        let config = ProjectConfig::default_for_project("demo");
        assert!(!config.broker.proactive_prefetch, "default must be false");
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert!(
            !reloaded.broker.proactive_prefetch,
            "default false must survive round-trip"
        );
    }

    /// F3-B-3: default_for_project uses conservative defaults for both fields.
    #[test]
    fn f3b_default_for_project_uses_conservative_defaults() {
        let config = ProjectConfig::default_for_project("demo");
        // answer_grade_min_score: 0.92 (rare, only very high-confidence capsules)
        assert!(
            (config.broker.answer_grade_min_score - 0.92).abs() < f32::EPSILON,
            "default answer_grade_min_score must be 0.92"
        );
        // proactive_prefetch: false (opt-in — never changes default behaviour)
        assert!(
            !config.broker.proactive_prefetch,
            "default proactive_prefetch must be false"
        );
    }

    // ── S3: SyncSection tests ──────────────────────────────────────────────

    /// S3-cfg-1: a pre-S3 project.toml (no `[sync]` section) loads cleanly
    /// and defaults to no sync dir and empty machine_id.
    #[test]
    fn s3_pre_s3_config_without_sync_loads_cleanly() {
        let toml = r#"
[kimetsu]
project_id = "demo"
schema_version = 7

[model]
provider = "anthropic"
model = "claude-opus-4-7"
api_key_env = "ANTHROPIC_API_KEY"
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

[broker.weights]
relevance = 0.5
confidence = 0.2
freshness = 0.2
scope = 0.1

[shell]
default_timeout_secs = 60
max_timeout_secs = 600
env_allowlist_extra = []
redact_secrets = true

[ingestion]
max_file_bytes = 524288
extra_skip_dirs = []
max_total_files = 50000

[run]
max_total_tool_calls = 60
max_total_model_turns = 30
max_total_cost_usd = 250.0
"#;
        let config = ProjectConfig::from_toml(toml).expect("pre-S3 toml must load");
        assert!(
            config.sync.dir.is_none(),
            "sync.dir must default to None when absent"
        );
        assert!(
            config.sync.machine_id.is_empty(),
            "sync.machine_id must default to empty string when absent"
        );
    }

    /// S3-cfg-2: a `[sync]` section with dir + machine_id round-trips cleanly.
    #[test]
    fn s3_sync_section_round_trips() {
        let mut config = ProjectConfig::default_for_project("demo");
        config.sync.dir = Some("/tmp/kimetsu-sync".to_string());
        config.sync.machine_id = "my-laptop-01".to_string();
        let serialized = config.to_toml().expect("serialize");
        let reloaded = ProjectConfig::from_toml(&serialized).expect("reload");
        assert_eq!(
            reloaded.sync.dir,
            Some("/tmp/kimetsu-sync".to_string()),
            "sync.dir must round-trip"
        );
        assert_eq!(
            reloaded.sync.machine_id, "my-laptop-01",
            "sync.machine_id must round-trip"
        );
    }

    /// S3-cfg-3: default_for_project gives an unconfigured sync section.
    #[test]
    fn s3_default_for_project_sync_unconfigured() {
        let config = ProjectConfig::default_for_project("demo");
        assert!(config.sync.dir.is_none(), "default sync.dir must be None");
        assert!(
            config.sync.machine_id.is_empty(),
            "default sync.machine_id must be empty"
        );
    }
}
