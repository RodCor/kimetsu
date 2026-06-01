# How Kimetsu Works

Kimetsu is a sidecar brain for coding agents. It runs alongside supported
host agents through MCP (including Claude Code and Codex), or as a standalone
chat REPL. It watches what the model does, learns which memories actually
help, and feeds higher-signal context into future runs. This document explains
the moving parts, in the order you'll encounter them.
---

## 1. Two ways to use it

**As a sidecar via MCP.** Run `kimetsu mcp serve` directly, or let
`kimetsu plugin install <target>` write the host config for you. The host
agent gets `kimetsu_*` tools (brain context + record, citations, memory
add/list/blame/conflicts, repo ingest, the bridge to other supported hosts).
Memories carry across sessions; learning compounds.

The intended loop is two calls: **`kimetsu_brain_context`** early on a
non-trivial task (zero overhead when the brain has nothing — it returns
`skipped: true`), then **`kimetsu_brain_record`** after solving a
non-obvious problem worth remembering. Supported host integrations can fire
the context step automatically: `kimetsu plugin install claude` writes
`.claude/settings.json`, and `kimetsu plugin install codex` writes
`.codex/hooks.json`. Both wire `UserPromptSubmit` to
`kimetsu brain context-hook`; hosts with a supported stop event also wire
`kimetsu brain stop-hook` to summarize what was captured (see section 7).

**As a standalone REPL.** Run `kimetsu chat`. Same brain, same
tools, just without a host harness. Useful for debugging a brain or
running short tasks where you don't want a second agent in the loop.

The CLI also has admin commands (`kimetsu brain ...`,
`kimetsu doctor`, `kimetsu bridge ...`) that you'll use for
maintenance — described below.

---

## 2. The brain

Everything kimetsu remembers lives in **brain.db**, a SQLite
database. Each project gets one at `<project>/.kimetsu/brain.db`. A
global user brain at `~/.kimetsu/brain.db` holds memories that
follow you across projects (set `KIMETSU_USER_BRAIN=0` to disable).

The brain is event-sourced. Every run writes a trace of `Event` rows
(JSONL). A **projector** turns those events into materialized tables
the broker can query fast:

- `runs` — one row per agent run (started_at, terminal_kind, cost).
- `events` — every event ever written, raw.
- `memories` — the durable knowledge. Each row carries scope
  (`global_user`, `project`, `repo`, `run`), kind (`preference`,
  `convention`, `command`, `failure_pattern`, `fact`), text, confidence,
  use_count, usefulness_score, and last_useful_at.
- `memory_proposals` — pending suggestions awaiting human review.
- `memory_citations` — which memories the model cited during which
  run, on which turn.
- `memory_conflicts` — ingest-time hits where a new memory's
  embedding was too close to an existing one with contradictory text.
- `repo_files`, `repo_files_fts`, `repo_manifests`,
  `repo_manifests_fts` — file-level indexes built by
  `kimetsu brain ingest repo`.
- `memories_fts` — FTS5 index of memory text for lexical retrieval.

The schema is forward-additive — every column added since v0.1 uses
`add_column_if_missing`, so an older brain.db opens cleanly under a
newer binary without rebuilds.

### Memory kinds

| Kind | Use |
|------|-----|
| `preference` | User-stated style choices ("prefer thiserror") |
| `convention` | Repo conventions ("always run cargo fmt") |
| `command` | Useful shell incantations ("regen with `cargo xtask gen`") |
| `failure_pattern` | "Don't do X, it caused Y last time" |
| `fact` | Domain knowledge — APIs, gotchas, architectural notes |

### Memory scopes

| Scope | Lives | Use |
|-------|-------|-----|
| `run` | This run only | Ephemeral notes — discarded at end |
| `repo` | This repo | Project conventions, code-specific facts |
| `project` | This project (== repo today) | Synonym for repo |
| `global_user` | User-wide brain | Personal preferences, cross-project knowledge |

---

## 3. The broker

When a run starts (chat REPL, MCP `kimetsu_brain_context` call, or
the agent loop's pre-stage hook), the **broker** assembles a
context bundle. It walks both brains, scores candidates, and returns
the top-N inside a token budget.

The score is a weighted sum of four signals, plus two multipliers:

```
raw_relevance  = (1 - α) * lexical_match + α * cosine_similarity
                                                 (where α = 0.5 default,
                                                  cosine only fires when
                                                  --features embeddings is on)

multiplier     = usefulness_multiplier(usefulness_score, use_count)
                 ∈ [0.5, 1.5]  blended by Bayesian smoothing

decay          = exp(-ln 2 · age_days / half_life_days)   ∈ [0, 1]
                 age measured from coalesce(last_useful_at, created_at)
                 half_life_days default = 30, set to 0 to disable

effective      = 1.0 + (multiplier - 1.0) · decay
                 (decay attenuates the *deviation* from neutral, not
                  the multiplier itself — a year-old +1.5 memory
                  slides toward 1.0, NOT toward 0)

final_score    = weights.relevance   · raw_relevance
               + weights.confidence  · confidence
               + weights.freshness   · freshness
               + weights.scope       · scope_weight
                 — all per-stage tunable via [broker.weights.<stage>]
```

Stages: `localization`, `patch_plan`, `verification`, `review`. Each
has its own weight profile in `project.toml`. The broker also runs
**MMR re-ranking** (lambda=0.7) to penalize within-kind redundancy —
two memories that mention the same words don't both crowd the
budget.

### Embeddings vs lean builds

- **Embeddings** (default for the CLI): `cargo install kimetsu-cli`
  ships with `--features embeddings` on. Pulls fastembed-rs + ONNX
  runtime; needs the VS2022 C++ runtime on Windows (ort prebuilts).
  Default model is BGE-small-en-v1.5. Cosine retrieval, semantic
  dedup, and conflict detection all light up. The ~24 MB model
  downloads to `~/.cache/huggingface/` on first embed call, then
  caches.
- **Choosing the model.** Three built-ins are curated:
  `bge-small-en-v1.5` (384d, default), `bge-m3` (1024d, multilingual),
  and `jina-v2-base-code` (768d, code-tuned). Resolution precedence is
  `KIMETSU_BRAIN_EMBEDDER` env > the `[embedder]` table in
  `project.toml` > default. Inspect/switch with `kimetsu brain model
  list` / `kimetsu brain model set <id>` (or the `kimetsu_brain_model_list`
  / `kimetsu_brain_model_set` MCP tools). Switching changes the vector
  dimension, so `model set` re-embeds the corpus with the new model;
  cross-model rows fall back to FTS until reindexed, so retrieval never
  breaks mid-migration.
- **Lean**: `cargo install kimetsu-cli --no-default-features`. No
  embedder binary, no model download. Retrieval is FTS-only via the
  `α=0` effective behavior. Semantic dedup and conflict detection at
  ingest become silent no-ops. The library crates
  (`kimetsu-brain`, `kimetsu-chat`) default to lean so downstream
  consumers stay slim; only the `kimetsu-cli` binary opts embeddings
  in by default.

---

## 4. Citations + blame

The model's biggest signal is *which memories it actually used*.
When a memory shows up in context but the model never reaches for
it, that's "silent passenger" data. When the model explicitly cites
a memory before solving the problem, that's strong evidence the
memory helped.

The flow:

1. The broker injects N capsules into the prompt (recorded as a
   `context.injected` event).
2. The model, mid-run, calls the **`cite_memory`** tool when it
   leans on a specific memory. Multiple cites per turn are fine.
3. At run end, the transport (chat REPL or harbor binary) emits one
   `memory.cited` event per citation. The projector mirrors each
   into `memory_citations`.
4. On `run.finished`, the projector applies the usefulness deltas:
   - **Cited memories**: ±1.0 (strong signal). Also bumps
     `last_useful_at` to "now" on success.
   - **Silent passengers**: ±0.1 (weak signal). Outcome-correlated,
     but small.
   - `run.failed` with `category = "Gate"` is treated as no signal —
     the agent's verifier failing isn't the memory's fault.

The split keeps the strong signal aimed at memories that actually
contributed, while still giving silent passengers a small +/- so the
broker can prune long-untouched noise.

### Inspecting a run

```bash
kimetsu brain memory blame <run-id>
```

Prints cited memories first (with rationale + turn) and silent
passengers second. `--json` for CI / hooks. The same surface is
available as the `kimetsu_brain_memory_blame` MCP tool so a host
agent can introspect its own runs.

---

## 5. Decay

A memory that was useful 6 months ago shouldn't outrank one that
was useful yesterday. The `last_useful_at` column (bumped only on
cited + run.finished) is the reference timestamp; the broker
applies a half-life curve to attenuate stale usefulness boosts.

- Default half-life: **30 days**.
- Configurable per-project:
  ```toml
  [broker.weights]
  decay_half_life_days = 14   # faster-moving repo
  # or
  decay_half_life_days = 0    # disable decay entirely
  ```
- Decay attenuates the *deviation from neutral*, not the
  multiplier itself: a max-boosted +1.5 memory decays toward 1.0
  (neutral, like a brand-new memory), not toward 0. Losing confidence
  in old signal shouldn't penalize a memory below one with zero
  history.

---

## 6. Semantic dedup + conflict detection at ingest

When a new capsule arrives via `kimetsu_brain_record` (the capture
tool host agents call after solving something), kimetsu first runs
**semantic dedup** through `propose_or_merge_memory`:

1. **Exact dup** — identical normalized text in the same scope/kind
   short-circuits; the existing memory's use_count bumps, nothing new
   is written.
2. **Near dup** — if an existing memory in scope is within cosine
   ≥ 0.85, the new text is *merged into* it (appended as "Also: …")
   rather than creating a near-twin. This is what stops a brain from
   filling with ten rephrasings of the same lesson over a gauntlet.
3. **High confidence** (≥ 0.7) with no near-dup → accepted directly.
4. **Lower confidence** → filed as a proposal for human review.

Dedup is embedder-gated; the lean build skips steps 1-2's semantic
match and falls through to accept/propose.

Separately, when `add_memory` writes a capsule, kimetsu scans the
active brain for nearby *contradictions*: existing memories in the
same scope whose embedding is close (cosine ≥ 0.8 by default) but
whose normalized text differs.

Hits land in `memory_conflicts`. The write itself is **not blocked**
— surfacing > blocking, because a blocked write loses user intent.
Instead the operator reviews via:

```bash
kimetsu brain memory conflicts                  # list open conflicts
kimetsu brain memory conflicts --resolve <id> kept_new
kimetsu brain memory conflicts --resolve <id> kept_existing
kimetsu brain memory conflicts --resolve <id> kept_both
```

`kept_new` invalidates the existing memory. `kept_existing`
invalidates the new one. `kept_both` is the legitimate case where
both apply (e.g., "use anyhow in CLI binaries" + "use thiserror in
library crates" — same shape, different scope semantically).

Conflict detection is **embedder-gated**. The lean build silently
skips it; build with `--features embeddings` to enable.

The MCP surface (`kimetsu_brain_memory_conflicts`) is read-only by
design. Resolution is CLI-only to keep the audit trail centralized —
an agent shouldn't silently "resolve" a real contradiction it
should have surfaced.

---

## 7. The MCP surface

Run `kimetsu mcp serve` and the host harness gets ~28
`kimetsu_*` tools. The ones you'll actually reach for:

| Tool | What it does |
|------|--------------|
| `kimetsu_brain_context` | Retrieve a context bundle for a query/stage (returns `skipped: true` when nothing relevant — zero overhead) |
| `kimetsu_brain_record` | Capture a lesson after a non-obvious solve; runs semantic dedup (§6) |
| `kimetsu_brain_status` | Brain health + memory counts at a glance |
| `kimetsu_brain_memory_add` | Persist a new memory directly |
| `kimetsu_brain_memory_list` | List memories in scope, sorted by relevance |
| `kimetsu_brain_memory_top` | Top memories by usefulness ratio |
| `kimetsu_brain_memory_proposals` | Pending proposals awaiting review (paginated: `limit`/`offset`) |
| `kimetsu_brain_memory_accept` / `_reject` | Promote / reject a proposal |
| `kimetsu_brain_memory_invalidate` | Retire a memory |
| `kimetsu_brain_memory_search` | Full-text search over memory text (paginated; filter by kind/scope) |
| `kimetsu_brain_memory_blame` | Per-run citation attribution |
| `kimetsu_brain_memory_conflicts` / `kimetsu_brain_conflict_resolve` | List / settle open ingest conflicts |
| `kimetsu_brain_prune` | List (or, with `apply`, invalidate) net-negative memories |
| `kimetsu_brain_model_list` / `kimetsu_brain_model_set` | Inspect / switch the embedding model (set re-embeds the corpus) |
| `kimetsu_brain_reindex` | Backfill stale/missing embeddings |
| `kimetsu_brain_config_show` | Read the parsed `project.toml` |
| `kimetsu_brain_ingest_repo` | Index repo files + manifests |
| `kimetsu_benchmark_context` | Retrieve a task-aware playbook (biases toward `semantic_operator` + `anti_pattern` roles) |
| `kimetsu_benchmark_record_outcome` | Record run outcome → proposal |
| `kimetsu_bridge_status` / `_export` / `_import` / `_sync` | Cross-harness skill registry + install/sync |
| `kimetsu_skills_search` / `kimetsu_skill` | Find / invoke a portable skill |
| `cite_memory` | (in-run) Mark a memory as cited |

Tool input schemas + descriptions are advertised via the standard
MCP `tools/list`. Every kimetsu_* tool returns `{"ok": true, "usage":
{...}, ...}` so the host agent gets actionable "how to use this
output" guidance, not just raw data.

### Host hooks

The MCP tools work whether or not the model decides to call them. To
make the loop reliable, Kimetsu's plugin installers write host-native
hook config:

- **Claude Code**: `.claude/settings.json`
- **Codex**: `.codex/hooks.json`

The core hook pattern is the same across hosts:

- **`UserPromptSubmit` → `kimetsu brain context-hook`** fires before
  each turn. It reads the prompt from stdin, retrieves a context
  bundle, and injects it — so the model sees relevant memories without
  having to remember to ask. Zero-overhead: when the brain has nothing,
  the hook emits nothing.
- **`Stop` → `kimetsu brain stop-hook`** fires when the host supports a
  stop event. It walks the transcript, counts `kimetsu_brain_record`
  calls, and prints a one-line post-turn banner — either confirming how
  many lessons were captured or nudging the model to record one after a
  non-trivial, un-captured session.

These are plain CLI subcommands, so the same pattern works under any
harness that can run a command on a prompt, stop, or tool event. The
Codex installer wires prompt-time context and proactive tool hooks; the
Claude Code installer wires prompt-time context, stop summaries, and
proactive tool hooks.

### Proactive recall (mid-work)

`UserPromptSubmit` only fires between turns. v0.8 adds two **tool-level**
hooks so the brain can surface a memory *while* the agent works — the
way a memory "comes to you" rather than you going to fetch it. Both use
a `matcher: "Bash"` so they only fire on shell commands (most tool calls
spawn nothing), and both emit `hookSpecificOutput.additionalContext`
without ever blocking:

- **`PreToolUse` → `kimetsu brain pretool-hook`** runs *before* a Bash
  command; if the command strongly matches a stored `failure_pattern` /
  `convention`, it warns first — heading off a known mistake.
- **`PostToolUse` → `kimetsu brain posttool-hook`** runs *after* a Bash
  command; when the output looks like a failure, it surfaces a matching
  `failure_pattern` / `command` fix.

Discipline keeps this near-zero-cost and non-spammy: retrieval is
**lexical-FTS-only** (no embedding-model load, so the per-call latency
stays low), gated by a **high score floor** (0.45; 0.35 once a
**repeated** failing command is detected), capped at **one capsule**,
**deduped per session** (a memory surfaces at most once), and
**throttled** by a refractory window between injections. When nothing
clears the bar the hook prints nothing — zero tokens. Per-session state
(surfaced ids, last injection, recent commands) lives in
`<repo>/.kimetsu/proactive/<session_id>.json` and is GC'd after 7 days.

Proactive hooks install by default with `kimetsu plugin install`; pass
`--no-proactive` (or `proactive:false` to `kimetsu_plugin_install`) to
skip `PreToolUse` / `PostToolUse` and keep only prompt-time context plus any
supported stop hook.

---

## 8. The bridge

Kimetsu also runs as a **cross-harness skill bridge**. The
`kimetsu bridge` subcommand:

- Discovers skills installed in supported hosts such as Claude Code,
  Codex, and the local kimetsu installation.
- Exports a chosen skill into another harness (e.g., move a skill from
  one host to another).
- Maintains a unified skill registry so the same skill works in
  any host.

`kimetsu bridge status` shows what's installed where; `kimetsu
bridge export <skill> --to <target>` does the install.

---

## 9. Doctor

`kimetsu doctor` is the wire-health check. Validates that every
subsystem actually works against the current workspace + user state:

- Project paths resolve.
- brain.db opens + schema matches.
- User brain reachable (or correctly disabled).
- Embedder loads (or correctly defaults to NoopEmbedder on lean).
- MCP server can spawn (skipped with `--skip-mcp` for sandboxes).
- Bridge can scan host harnesses.

Hermetic by default — safe in CI. JSON output (`--json`) for
hooks. Run after upgrading kimetsu or whenever something looks off.

---

## 10. Configuration

Project config lives in `<project>/project.toml`:

```toml
[kimetsu]
project_id = "my-project"
schema_version = 1

[model]
provider = "anthropic"        # or "claude_code"
model = "claude-opus-4-7"
api_key_env = "ANTHROPIC_API_KEY"
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

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
```

Environment variables that override at runtime:

| Variable | Effect |
|----------|--------|
| `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN` / `OPENAI_API_TOKEN` | Provider credentials |
| `KIMETSU_USER_BRAIN=0` | Disable the user brain (project-only memories) |
| `KIMETSU_BRAIN_EMBEDDER=noop\|bge\|jina-v2-base-code\|...` | Pick the embedder (or disable) |

---

## 11. What kimetsu is NOT

- It's not a model. It runs through a host agent or configured model provider
  (for example Anthropic API or Claude Code OAuth).
- It's not a sandbox. Tools run on the host machine.
- It's not a vector DB. The brain is SQLite + FTS5 + optional cosine.
  Single file per project. Backups are `cp brain.db`.

---

## 12. Where to go next

- Run `kimetsu doctor` to verify your install.
- Read the **CHANGELOG** for the per-version history — this doc
  describes how kimetsu works today; the CHANGELOG tells you when each
  piece landed.
- Look at the per-crate `src/lib.rs` doc comments for module-level
  detail (`kimetsu-brain`, `kimetsu-agent`, `kimetsu-chat`,
  `kimetsu-cli`, `kimetsu-core`).
- For anything benchmark / impact-measurement related, the bench
  surface lives in a separate internal repo — that's by design
  (see "Embeddings vs lean builds" above).
