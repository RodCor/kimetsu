## The bridge

Kimetsu also runs as a **cross-harness skill bridge**. The
`kimetsu bridge` subcommand:

- Discovers skills installed in supported hosts such as Claude Code,
  Codex, Pi, OpenClaw, and the local kimetsu installation.
- Exports a chosen skill into another harness (e.g., move a skill from
  one host to another).
- Maintains a unified skill registry so the same skill works in
  any host.

`kimetsu bridge status` shows what's installed where; `kimetsu
bridge export <skill> --to <target>` does the install.

---

## Doctor

`kimetsu doctor` is the wire-health check. Validates that every
subsystem actually works against the current workspace + user state:

- Project paths resolve.
- brain.db opens + schema matches.
- User brain reachable (or correctly disabled).
- Embedder loads (or correctly defaults to NoopEmbedder on lean).
- MCP server can spawn (skipped with `--skip-mcp` for sandboxes).
- Bridge can scan host harnesses.

Hermetic by default, safe in CI. JSON output (`--json`) for
hooks. Run after upgrading kimetsu or whenever something looks off.

**`kimetsu doctor --selftest`** is the one-shot end-to-end proof: in a
throw-away temp project (it never touches your real workspace or user brain) it
records a sample memory and retrieves it back, printing a
`✓ recorded a memory and retrieved it` success line and exiting non-zero
if any step fails. Use it to confirm a fresh install actually works.

---

## Configuration

Project config lives in `<project>/project.toml`:

```toml
[kimetsu]
project_id = "my-project"
schema_version = 1            # project.toml CONFIG-format version, NOT the
                              # brain.db schema version (that is migrated
                              # separately; see §2 "Durable upgrades")
use_user_brain = true         # false → per-project opt-out of the global brain

[model]
provider = "anthropic"        # or "claude_code", "openai", "bedrock"
model = "claude-opus-4-7"     # bedrock: the full id, e.g. anthropic.claude-3-5-...
api_key_env = "ANTHROPIC_API_KEY"
region_env = "AWS_REGION"     # bedrock only (also reads AWS_DEFAULT_REGION)
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[embedder]
enabled = true                # false → FTS-only, no vectors written or queried
model = "bge-small-en-v1.5"   # or "bge-m3", "jina-v2-base-code"

[broker]
default_budget_tokens = 6000  # flat fallback; the adaptive budget supersedes it
ambient = true                # false → don't append workspace context to queries
max_capsules = 8              # hard cap on capsules rendered into a prompt
min_semantic_score = -1.0     # AUTO (bge: 0.35, others: off); >0 sets an explicit floor
budget_floor_tokens = 1500    # adaptive-budget floor (small tasks not starved)
budget_run_cap_tokens = 8000  # per-run ceiling on brain-injected tokens
compress_capsules = true      # v1.5: compress rendered capsule text (strips tags/context
                              # annotations, caps at 3 sentences); ranking unaffected
session_dedupe = true         # v1.5: skip capsules already injected this session
warm_start = true             # v2.0: SessionStart injects the project digest + episodic
                              # resume (kimetsu brain session-start-hook)
answer_grade_min_score = 0.92 # v2.0: a top capsule scoring >= this gets a "Verified answer
                              # from project memory:" prefix; >1.0 disables. Ranking untouched.
proactive_prefetch = false    # v2.0: opt-in trajectory-based pre-fetch at PreToolUse

[storage]
backend = "flat"              # v2.0: "flat" (FTS5 + usearch ANN, default) |
                              # "graph-lite" (+ typed-edge 1-2 hop expansion over memory_edges) |
                              # "graph" (in-memory petgraph; kimetsu-remote only). Switching
                              # re-projects from the event log, no data migration.

[cheap_model]                 # v2.0: ONE optional model for digest / resume / skill-draft /
                              # `kimetsu ask` / distiller / consolidation. Absent = every
                              # consumer degrades gracefully (rule-based / FTS-only / refuse).
enabled = false
provider = "ollama"           # "ollama" (local, no key) | "anthropic" | "openai" | "bedrock"
model = "qwen2.5:3b"          # ollama recs: qwen2.5:3b, llama3.2:3b. anthropic: claude-haiku-4-5
api_key_env = "ANTHROPIC_API_KEY"   # NAME of the env var; not required for ollama
base_url_env = "OLLAMA_BASE_URL"    # ollama default: http://localhost:11434/v1
# Back-compat: if [cheap_model] is absent, an existing [learning.distiller] is used.

[sync]                        # v2.0: server-less multi-machine sync (event-log replication)
dir = ""                      # a shared folder (Dropbox/Syncthing/NAS); empty = off
machine_id = ""               # stable per-machine id; empty = generated

[broker.weights]
relevance = 0.50
confidence = 0.20
freshness = 0.20
scope = 0.10
decay_half_life_days = 30.0   # 0 to disable

[broker.weights.localization]
relevance = 0.70              # heavier on relevance for the localization stage
confidence = 0.10
freshness = 0.10
scope = 0.10

# similar overrides for [broker.weights.patch_plan],
# [broker.weights.verification], [broker.weights.review]

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
store_queries = true        # v1.5: include raw query text in context.served telemetry
                            # (on-machine only; powers the personal eval set for brain tune)
                            # set false to revert to query-hash-only (pre-v1.5 behavior)

[learning.distiller]
enabled = false
provider = "anthropic"        # or "openai", "bedrock"
model = "claude-haiku-4-5"    # OpenAI default: "gpt-5.4-mini"
api_key_env = "ANTHROPIC_API_KEY"   # or "OPENAI_API_KEY"
base_url_env = "ANTHROPIC_BASE_URL" # or "OPENAI_BASE_URL"
```

The agent model and the distiller are configured **independently**, so the
provider can differ. For example, run the agent on **AWS Bedrock** (Anthropic models via
the InvokeModel API, SigV4-signed from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
(+ optional `AWS_SESSION_TOKEN`) and `AWS_REGION`) while the harvester stays on
direct Claude or OpenAI.

**Bidirectional config (off-switches).** Every optional feature is turn-off-able
in `project.toml` and honored at runtime with precedence
**env override > config > default**. Every field is `#[serde(default)]`, so a
partial `project.toml` loads cleanly: older files gain the new defaults on
upgrade. The toggles: `[embedder] enabled`, `[broker] ambient`,
`[kimetsu] use_user_brain`, plus the already-bidirectional `[learning]
auto_harvest`, `[learning.distiller] enabled`, and `[shell] redact_secrets`.
v2.0 adds `[broker] warm_start`, `[broker] proactive_prefetch`,
`[broker] answer_grade_min_score`, `[storage] backend`, `[cheap_model] enabled`,
and `[sync] dir`, all defaulting to the pre-v2.0 behavior (warm-start on but
no-op without memories/episodes; backend `flat`; cheap-model off → graceful
degradation). One honest caveat: the warm-start **digest is currently
rule-based**: the `[cheap_model]` distillation hook-point exists but does not
yet call the model; `kimetsu ask`, resume, and skill-draft do use it.
Flip any of them with **`kimetsu config edit`** (opens `$EDITOR` on
`project.toml` and re-validates on save); a re-install *merges*, so your toggles
survive.

Environment variables that override the matching config field at runtime
(env > config > default). Each now has a persistent `project.toml` equivalent:

| Variable | Effect |
|----------|--------|
| `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN` / `OPENAI_API_KEY` / `AWS_ACCESS_KEY_ID`+`AWS_SECRET_ACCESS_KEY`+`AWS_REGION` | Provider credentials (incl. AWS Bedrock) |
| `KIMETSU_USER_BRAIN=0` | Disable the user brain (= `[kimetsu] use_user_brain = false`) |
| `KIMETSU_BRAIN_EMBEDDER=noop\|bge\|jina-v2-base-code\|...` | Pick the embedder, or disable it (= `[embedder] enabled = false` / `model`) |
| `KIMETSU_BRAIN_AMBIENT=off` | Disable ambient workspace context (= `[broker] ambient = false`) |

---

## What kimetsu is NOT

- It's not a model. It runs through a host agent or a configured model provider
  (Anthropic API, Claude Code OAuth, OpenAI, or AWS Bedrock).
- It's not a sandbox. Tools run on the host machine.
- It's not an external vector DB. The brain is still a single SQLite file per
  project (FTS5 + optional cosine). On the embeddings build the semantic index
  is a usearch HNSW sidecar (`brain.usearch`) next to brain.db: no separate vector store,
  no service to run. Backups are still `cp brain.db` (and the brain also
  auto-backs-up to a `brain.db.bak-*` sidecar before any schema migration).

---

## Where to go next

- Run `kimetsu doctor` to verify your install.
- Read the **CHANGELOG** for the per-version history. This doc
  describes how kimetsu works today; the CHANGELOG tells you when each
  piece landed.
- Look at the per-crate `src/lib.rs` doc comments for module-level
  detail (`kimetsu-brain`, `kimetsu-agent`, `kimetsu-chat`,
  `kimetsu-cli`, `kimetsu-core`).
- For anything benchmark / impact-measurement related, the bench
  surface lives in a separate internal repo, which is by design
  (see "Embeddings vs lean builds" above).
