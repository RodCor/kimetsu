Every knob in `project.toml`, the off-switches, and the environment overrides.

Project config lives in `<project>/project.toml`:

```toml
[kimetsu]
project_id = "my-project"
schema_version = 1            # config-format version, NOT the brain.db schema
use_user_brain = true         # false: per-project opt-out of the global brain
# tier = "free"               # omit for auto; see "Free and Deep" below

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
backend = "graph-lite"        # "flat" | "graph-lite" | "graph" (remote only).
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

## Free and Deep

`[kimetsu] tier` picks which of two pipelines the brain runs.

**Free** is the default and what the published benchmarks measure: **zero LLM
calls anywhere in the memory pipeline.** Ingest, store, retrieve and rerank are
FTS5 + local embeddings + a local cross-encoder, and every capability has a
deterministic or statistical implementation.

**Deep** adds a local small model to the handful of features that are genuinely
better with one. Every Deep feature falls back to the Free behaviour, so
switching tiers can add quality but never removes a capability:

| Feature | Free | Deep |
|---------|------|------|
| write-time lesson capture | rule-based | model-distilled lessons |
| repo digest | rule-based assembly | model-distilled summary |
| reflection over memory clusters | not run | synthesized general principles |
| contradiction detection | cosine proximity | entailment adjudication |
| HyDE query expansion | not run | hypothetical-answer expansion |

Leave `tier` unset for **auto**: a brain with a cheap model configured is
already making model calls, so it resolves to Deep; a brain without one
resolves to Free. That keeps every pre-v3.0 config behaving exactly as before.
Set it explicitly to force the choice — `tier = "free"` is a durable opt-out of
model calls even when credentials are present, and `tier = "deep"` with no
reachable model resolves back down to Free and says so in `kimetsu doctor` and
`kimetsu brain status`.

Commands you invoke by name (`kimetsu ask`, `brain reflect`, `brain distill`,
`brain skills --draft`) are explicit requests, not pipeline behaviour, and are
not gated by the tier.

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
| `KIMETSU_TIER=free\|deep` | Force the product tier for this process |
