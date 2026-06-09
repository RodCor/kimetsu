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
}

impl ProjectConfig {
    pub fn default_for_project(project_id: impl Into<String>) -> Self {
        Self {
            kimetsu: KimetsuSection {
                project_id: project_id.into(),
                schema_version: KIMETSU_CONFIG_VERSION,
                use_user_brain: default_true(),
            },
            model: ModelSection::default(),
            broker: BrokerSection::default(),
            shell: ShellSection::default(),
            ingestion: IngestionSection::default(),
            run: RunSection::default(),
            embedder: EmbedderSection::default(),
            learning: LearningSection::default(),
        }
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
    /// ranking stage. One of the curated fastembed reranker ids
    /// (`jina-reranker-v1-turbo-en` default, `bge-reranker-base`,
    /// `bge-reranker-v2-m3`, `jina-reranker-v2-base-multilingual`) or
    /// `"off"` to disable reranking. `#[serde(default = …)]` keeps older
    /// configs loading with the reranker on.
    #[serde(default = "default_reranker_id")]
    pub reranker: String,
}

fn default_embedder_id() -> String {
    "bge-small-en-v1.5".to_string()
}

fn default_reranker_id() -> String {
    "jina-reranker-v1-turbo-en".to_string()
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
    /// Default 0.35 (v1.0.0, was 0.0/disabled): with the warm daemon now
    /// serving semantic retrieval to every prompt, the floor needs teeth by
    /// default — bge-family cosines for genuinely related pairs sit well
    /// above ~0.5 while unrelated pairs cluster below ~0.4, so 0.35 trims
    /// clear noise without starving recall. Tune against `kimetsu brain
    /// eval`; set 0.0 to restore the old keep-everything behaviour.
    /// `#[serde(default = …)]` keeps older configs loading with the floor on.
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
}

fn default_max_capsules() -> usize {
    8
}

fn default_min_semantic_score() -> f32 {
    0.35
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
}

impl Default for IngestionSection {
    fn default() -> Self {
        Self {
            max_file_bytes: 524_288,
            extra_skip_dirs: Vec::new(),
            max_total_files: 50_000,
            detect_conflicts: true,
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
        assert_eq!(config.embedder.model, "bge-small-en-v1.5");
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
        assert_eq!(config.broker.min_semantic_score, 0.35);
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
        assert!(config.embedder.daemon, "embedder.daemon must default to true");
        assert!(
            config.embedder.warm_on_start,
            "embedder.warm_on_start must default to true"
        );
        // v1.0.0: reranker defaults to jina-reranker-v1-turbo-en so existing
        // installs gain the cross-encoder ranking stage on upgrade.
        assert_eq!(
            config.embedder.reranker,
            "jina-reranker-v1-turbo-en",
            "embedder.reranker must default to jina-reranker-v1-turbo-en"
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
}
