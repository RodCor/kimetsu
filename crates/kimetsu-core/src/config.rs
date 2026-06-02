use serde::{Deserialize, Serialize};

use crate::{KIMETSU_SCHEMA_VERSION, KimetsuResult};

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
                schema_version: KIMETSU_SCHEMA_VERSION,
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
}

/// v0.8: embedding-model selection. `model` is one of the curated
/// built-in ids exposed by `kimetsu brain model list`
/// (`bge-small-en-v1.5`, `bge-m3`, `jina-v2-base-code`). Switching
/// changes the vector dimension, so a `kimetsu brain reindex` is
/// required for cosine retrieval to use the new model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedderSection {
    #[serde(default = "default_embedder_id")]
    pub model: String,
}

fn default_embedder_id() -> String {
    "bge-small-en-v1.5".to_string()
}

impl Default for EmbedderSection {
    fn default() -> Self {
        Self {
            model: default_embedder_id(),
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
/// non-secret selection lives here.
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

impl Default for DistillerSection {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_distiller_provider(),
            model: default_distiller_model(),
            api_key_env: default_distiller_api_key_env(),
            base_url_env: default_distiller_base_url_env(),
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerSection {
    pub default_budget_tokens: u32,
    pub weights: BrokerWeights,
}

impl Default for BrokerSection {
    fn default() -> Self {
        Self {
            default_budget_tokens: 6000,
            weights: BrokerWeights::default(),
        }
    }
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
}

impl Default for IngestionSection {
    fn default() -> Self {
        Self {
            max_file_bytes: 524_288,
            extra_skip_dirs: Vec::new(),
            max_total_files: 50_000,
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
    }
}
