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
