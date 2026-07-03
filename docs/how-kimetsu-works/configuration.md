Every knob in `project.toml`, the off-switches, and the environment overrides.

Project config lives in `<project>/project.toml`:

```toml
[kimetsu]
project_id = "my-project"
schema_version = 1            # config-format version, NOT the brain.db schema
use_user_brain = true         # false: per-project opt-out of the global brain

[model]
provider = "anthropic"        # or "claude_code", "openai", "bedrock"
model = "claude-opus-4-8"     # bedrock: the full id
api_key_env = "ANTHROPIC_API_KEY"
region_env = "AWS_REGION"     # bedrock only
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[embedder]
enabled = true                # false: FTS-only, no vectors
model = "bge-small-en-v1.5"   # or "bge-m3", "jina-v2-base-code"

[broker]
default_budget_tokens = 6000  # flat fallback; the adaptive budget supersedes it
ambient = true                # false: no workspace context appended to queries
max_capsules = 8              # hard cap on capsules per prompt
min_semantic_score = -1.0     # AUTO (bge: 0.35, others: off); >0 sets a floor
budget_floor_tokens = 1500    # small tasks are never starved
budget_run_cap_tokens = 8000  # per-run ceiling on injected tokens
compress_capsules = true      # strip tags, cap at 3 sentences; ranking unaffected
session_dedupe = true         # skip capsules already injected this session
warm_start = true             # SessionStart injects the digest + episodic resume
answer_grade_min_score = 0.92 # top capsule >= this gets a "Verified answer" prefix
proactive_prefetch = false    # opt-in trajectory-based pre-fetch at PreToolUse

[storage]
backend = "flat"              # "flat" | "graph-lite" | "graph" (remote only).
                              # Switching re-projects from the event log.

[cheap_model]                 # one optional model for digest / resume / ask /
enabled = false               # distiller / consolidation; absent = graceful
provider = "ollama"           # rule-based degradation everywhere
model = "qwen2.5:3b"          # anthropic: claude-haiku-4-5
api_key_env = "ANTHROPIC_API_KEY"   # not required for ollama
base_url_env = "OLLAMA_BASE_URL"

[sync]                        # server-less multi-machine sync
dir = ""                      # a shared folder (Dropbox/Syncthing/NAS); empty = off
machine_id = ""               # stable per-machine id; empty = generated

[broker.weights]
relevance = 0.50
confidence = 0.20
freshness = 0.20
scope = 0.10
decay_half_life_days = 30.0   # 0 to disable
# per-stage overrides: [broker.weights.localization], .patch_plan,
# .verification, .review

[shell]
default_timeout_secs = 60
max_timeout_secs = 600
env_allowlist_extra = ["RUSTFLAGS", "CARGO_HOME"]
redact_secrets = true

[ingestion]
max_file_bytes = 524_288
extra_skip_dirs = []
max_total_files = 50_000

[run]
max_total_tool_calls = 60
max_total_model_turns = 30
max_total_cost_usd = 250.0    # advisory under subscription providers

[learning]
auto_harvest = true
store_queries = true          # raw query text in telemetry (on-machine only)

[learning.distiller]
enabled = false
provider = "anthropic"        # or "openai", "bedrock"
model = "claude-haiku-4-5"    # OpenAI default: "gpt-5.4-mini"
api_key_env = "ANTHROPIC_API_KEY"
base_url_env = "ANTHROPIC_BASE_URL"
```

The agent model and the distiller are configured independently: the agent can
run on AWS Bedrock while the harvester stays on direct Claude or OpenAI.

**Off-switches.** Every optional feature can be turned off in `project.toml`,
with precedence env override > config > default. Every field is
`#[serde(default)]`, so a partial or older file loads cleanly and gains new
defaults on upgrade. Edit with `kimetsu config edit` (opens `$EDITOR`,
re-validates on save); re-installs merge, so toggles survive.

Environment overrides:

| Variable | Effect |
|----------|--------|
| `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN` / `OPENAI_API_KEY` / AWS credentials | Provider credentials |
| `KIMETSU_USER_BRAIN=0` | Disable the user brain |
| `KIMETSU_BRAIN_EMBEDDER=noop\|bge\|jina-v2-base-code\|...` | Pick or disable the embedder |
| `KIMETSU_BRAIN_AMBIENT=off` | Disable ambient workspace context |
