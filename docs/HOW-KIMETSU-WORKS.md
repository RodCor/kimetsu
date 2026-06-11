# How Kimetsu Works

Kimetsu is a sidecar brain for coding agents. It runs alongside supported
host agents through MCP (including Claude Code and Codex), or as a standalone
chat REPL. It watches what the model does, learns which memories actually
help, and feeds higher-signal context into future runs. This document explains
the moving parts, in the order you'll encounter them.

## 1. Ways to use it

**As a sidecar via MCP.** Run `kimetsu mcp serve` directly, or let
`kimetsu plugin install <target>` write the host config for you. The host
agent gets `kimetsu_*` tools (brain context + record, citations, memory
add/list/blame/conflicts, repo ingest, the bridge to other supported hosts).
Memories carry across sessions; learning compounds.

The intended loop is two calls: **`kimetsu_brain_context`** early on a
non-trivial task (zero overhead when the brain has nothing — it returns
`skipped: true`), then **`kimetsu_brain_record`** after solving a
non-obvious problem worth remembering. `kimetsu plugin install <host>` wires the
context step automatically for **Claude Code**, **Codex**, **Pi**, and
**OpenClaw** — writing each host's native config (hooks + MCP for Claude/Codex/
OpenClaw; a TypeScript extension for Pi, which has no MCP). They wire
`UserPromptSubmit` to `kimetsu brain context-hook`; hosts with a supported stop
event also wire `kimetsu brain stop-hook` to summarize what was captured (see
section 7). Pi and OpenClaw are opt-in Cargo features, bundled in the official
prebuilt/npm binaries.

**As a standalone REPL.** Run `kimetsu chat`. Same brain, same
tools, just without a host harness. Useful for debugging a brain or
running short tasks where you don't want a second agent in the loop.

**As a shared server (Kimetsu Remote, beta).** Run the brain on a server and
connect over HTTP MCP — one brain per *repository*, shared across machines or a
team, with no local checkout. See §7a.

The CLI also has admin commands (`kimetsu brain ...`,
`kimetsu doctor`, `kimetsu bridge ...`) that you'll use for
maintenance — described below.

---

## 2. The brain

Everything kimetsu remembers lives in **brain.db**, a single SQLite
file. Each project gets one at `<project>/.kimetsu/brain.db`. A
global user brain at `~/.kimetsu/brain.db` holds memories that
follow you across projects (set `KIMETSU_USER_BRAIN=0`, or
`[kimetsu] use_user_brain = false` in `project.toml`, to disable).

`.kimetsu/` is deliberately **lean**: a brain-only install holds just
`brain.db` (plus its `-wal` / `-shm` and any `brain.db.bak-*` migration
sidecars) and `project.toml`. Memory writes persist straight to brain.db —
they do **not** create a per-write `runs/<id>/` directory. Only a real agent
run still writes a `runs/<id>/` dir with its artifacts. The transient
non-brain working dirs (`proactive/`, `chat/`, `bench/`) live OUT of the repo,
under `~/.kimetsu/cache/<project-hash>/`, so they never clutter your tree.

The brain is event-sourced, and the **`events` table inside brain.db is the
durable event log** — not a loose pile of JSONL files. A **projector** replays
those events into materialized tables the broker can query fast.
`kimetsu brain rebuild` re-derives every projection from the `events` table
(pass `--from-traces` to re-import from legacy on-disk `trace.jsonl` files for
recovery). The materialized tables:

- `runs` — one row per agent run (started_at, terminal_kind, cost).
- `events` — every event ever written, raw; the durable source for rebuild.
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

### Durable upgrades: schema migrations

brain.db carries a schema version (`KIMETSU_SCHEMA_VERSION`, currently **3**)
in its `schema_info` table. On every read-write open, a versioned,
forward-only migration runner brings the DB up to the binary's target. Each
migration runs inside **one transaction** (the DDL and the version bump commit
together), so a crash mid-upgrade leaves the DB cleanly stamped at an
intermediate version rather than half-applied. Before any version-advancing
migration the runner takes an online-backup snapshot to a
`brain.db.bak-<from>-<to>-<ts>` sidecar next to the DB (skipped for empty
brains — a fresh install has nothing to protect; the three newest backups are
kept). A read-only open of an un-migrated brain degrades gracefully — it reports
"needs migration" and the next read-write open performs it.

This DB schema version is **decoupled from the `project.toml` config version**
(`KIMETSU_CONFIG_VERSION`, still `1`). So `[kimetsu] schema_version = 1` in
`project.toml` is the *config-format* version, not the DB schema — the database
can evolve (and migrate) without forcing every project.toml to be rewritten.
The old "forward-additive `add_column_if_missing`, no rebuild" patches from
v0.1–v0.5 are now folded into the single v1→v2 migration.

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

**Candidate generation.** Lexical FTS5 always provides candidates. On the
embeddings build the broker *also* runs an approximate-nearest-neighbour query
against a **usearch HNSW** index (persisted as a `brain.usearch` sidecar next to
brain.db, f16-quantized by default, O(log N) per query) and **unions** those hits with the FTS
set — so a memory whose *meaning* matches the query can surface even when it
shares no words with it. Lean builds use the FTS candidate set alone.

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
has its own weight profile in `project.toml`.

**Selection (sharpened in v1.0).** On the embeddings build the broker runs
**embedding-MMR** (lambda=0.7): diversity is measured by cosine distance, so it
collapses true paraphrase near-duplicates that share no surface words (with
Jaccard token overlap as the lean-build / fallback path). An **absolute
semantic-relevance floor** (`min_semantic_score`, embeddings-only) then drops
candidates whose cosine to the query is below the threshold *before* budgeting —
so a genuinely off-topic query hits the zero-capsule "skipped" path and returns
nothing rather than padding the prompt with weak hits. Lean (FTS-only)
selection is unchanged.

Tunable knobs in `[broker]`:

- `max_capsules` (default **8**) — hard cap on capsules rendered into a prompt.
- `min_semantic_score` (default `0.0` = off) — the embeddings-only relevance
  floor described above.
- `budget_floor_tokens` (default `1500`) and `budget_run_cap_tokens`
  (default `8000`) — bounds for the adaptive per-run budget (see the agent
  brain section below).

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

## 3a. The agent brain (proactive + cost-shrinking)

The broker above describes *retrieval*. For the autonomous agent pipeline,
v1.0 layers an adaptive, task-aware recall strategy on top — so the brain is
proactive, and its token overhead grows far slower than the task does.

- **Task-kind routing.** Each task is classified once by a cheap deterministic
  keyword classifier into one of `Debug` / `Feature` / `Refactor` / `Docs` /
  `Investigation` (priority order on a tie: Debug > Investigation > Refactor >
  Docs > Feature). A task-kind weight layer composes over the per-stage weights
  (then renormalizes) to bias which *kinds* of memory get recalled: Debug leans
  on recent `failure_pattern`s, Refactor on `convention`/scope, Investigation
  on broad `fact`/`preference` recall. `Feature` is the neutral default — it
  leaves the stage weights untouched.
- **Proactive "Known pitfalls".** Before the first implementation attempt, a
  tight `failure_pattern`/`convention` retrieval surfaces known pitfalls —
  proactively, not only after a failure. It costs ~zero tokens when nothing
  matches, and a per-run recall ledger stops it re-surfacing the same pitfall
  on retries.
- **Cross-stage capsule dedup.** A capsule rendered in an earlier stage is
  back-referenced (not re-rendered) in later stages and counted once via the
  run's recall ledger — so brain overhead *shrinks* in relative terms as a
  task spans more stages.
- **Lazy capsule expansion.** Top-confidence capsules are injected in full; the
  long tail is injected as ~1-line headlines that the agent expands on demand
  via a new **`expand_capsule`** tool (it resolves `memory:` / `file:`
  handles). The agent only pays for the detail it actually opens.
- **Adaptive budget.** The flat 6000-tokens-per-stage budget is replaced by one
  that scales *sublinearly* with task size (`floor + k·√task_size`), floored by
  `budget_floor_tokens` so small tasks aren't starved and capped per-run by
  `budget_run_cap_tokens` via the ledger. Doubling task size grows the budget
  by only ~41%; a 5× task by ~124%. (When the size signal is unavailable the
  broker falls back to the flat `default_budget_tokens`.)

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

## 6a. Analytics — is the brain actually helping?

A brain you can't measure is a brain you can't trust. `kimetsu brain insights`
(and the `kimetsu_brain_insights` MCP tool) compute proof-of-value metrics
over a recent-runs window — default the last 50 runs, override with
`--last-n-runs N` or an ISO-8601 `--since`, and `--top N` sizes the ranked
lists. `--json` emits the full report for CI/dashboards. The metrics:

- **Retrieval hit-rate & skip-rate** — of the retrievals served, how many
  returned at least one capsule vs. hit the zero-capsule skipped path.
- **Citation rate** — what fraction of retrieved memories the model actually
  cited.
- **Proposal acceptance rate** — accepted / (accepted + rejected).
- **Usefulness trend** — summed usefulness, average usefulness ratio, and the
  net `run.finished − run.failed(non-Gate)` outcome over the window.
- **Harvest yield** — memories created in the window, broken down by source,
  and per-run yield.
- **Corpus health** — active vs. invalidated counts, breakdowns by scope/kind,
  top-useful memories, prune candidates, open conflicts, pending proposals.
- **Token economy** — average injected tokens and capsule count per retrieval.

These are backed by two event additions: a **`context.served`** event logs
*every* retrieval (hit or miss), and **`context.injected`** now carries the
injected-token count — so the hit-rate and token-economy numbers are real
counts, not estimates.

### ROI ledger

`kimetsu brain roi [--window 7d|30d|all] [--json]` estimates how much the
brain saved you. It sums conservative per-kind token credits for every cited
memory (failure_pattern=1500, command=400, convention=300, fact=500,
preference=200 tokens), subtracts injected-token overhead, and shows a
net-positive / net-negative verdict. Dollar values are shown when the active
model is in the built-in price table or when `[model] price_per_mtok` is set.
Honest negatives are displayed as-is. The Stop hook appends a per-session
savings line when ≥1 citation occurred. Methodology: `docs/ROI-METHODOLOGY.md`.

### Consolidation and triage

`kimetsu brain consolidate` merges near-duplicate memories that accumulated
different phrasings of the same lesson. Default threshold: cosine ≥ 0.92
within the same embedding model. The survivor keeps its id and text; members
get `superseded_by` set (schema v3) and a `memory.superseded` event so
`brain rebuild` reproduces the merge. Citations are reassigned to the survivor.
`--distill` adds a second pass: loose clusters (0.75–0.85 cosine, ≥3 memories,
≥1 shared tag) are fed to the configured distiller and the result lands as a
memory proposal for review. `--dry-run` prints the plan without writing.

`kimetsu brain triage` lists memories below a usefulness + age threshold
(defaults: score < 0.2 and last-useful > 30 days) and prompts keep / prune /
skip interactively. `--prune-all --yes` for non-interactive batch pruning.

### Self-tuning

`kimetsu brain tune --status` shows how many positive eval cases the brain has
accumulated (from `context.served` + citation joins). `kimetsu brain tune`
sweeps `broker.min_fts_coverage` × `broker.min_semantic_score` against the
production embedder and picks the combo that maximises the objective on the
training split. A holdout guardrail (deterministic 20% split) prevents
writing a config that regresses holdout quality. `--apply` writes only the
floor parameters to `project.toml`; `--revert` restores the previous entry.
Dry-run by default.

---

## 7. The MCP surface

Run `kimetsu mcp serve` and the host harness gets ~28
`kimetsu_*` tools. The ones you'll actually reach for:

| Tool | What it does |
|------|--------------|
| `kimetsu_brain_context` | Retrieve a context bundle for a query/stage (returns `skipped: true` when nothing relevant — zero overhead) |
| `kimetsu_brain_record` | Capture a lesson after a non-obvious solve; runs semantic dedup (§6) |
| `kimetsu_brain_status` | Brain health + memory counts at a glance |
| `kimetsu_brain_insights` | Effectiveness analytics over a recent-runs window (hit-rate, citation rate, acceptance, token economy — §6a) |
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
| `kimetsu_brain_cite` | (write-gated) Record that a retrieved memory materially helped — closes the ground-truth loop for self-tuning |
| `cite_memory` | (in-run) Mark a memory as cited |
| `expand_capsule` | (in-run) Expand a lazily-injected capsule headline to full detail by resolving its `memory:` / `file:` handle (§3a) |

Tool input schemas + descriptions are advertised via the standard
MCP `tools/list`. Every kimetsu_* tool returns `{"ok": true, "usage":
{...}, ...}` so the host agent gets actionable "how to use this
output" guidance, not just raw data.

### Host hooks

The MCP tools work whether or not the model decides to call them. To
make the loop reliable, Kimetsu's plugin installers write host-native
hook config:

- **Claude Code**: `.claude/settings.json` (hooks) + `.mcp.json` (MCP server)
- **Codex**: `.codex/hooks.json` + `.codex/config.toml`
- **OpenClaw**: `openclaw.json` (MCP server + a hooks plugin) + a `kimetsu-context` skill
- **Pi** (no MCP): a TypeScript extension under `~/.pi/agent/extensions/` that
  shells to `kimetsu brain *-hook`, plus a `kimetsu-brain` skill

The core hook pattern is the same across MCP hosts:

- **`UserPromptSubmit` → `kimetsu brain context-hook`** fires before
  each turn. It reads the prompt from stdin, retrieves a context
  bundle, and injects it — so the model sees relevant memories without
  having to remember to ask. Zero-overhead: when the brain has nothing,
  the hook emits nothing. On `embeddings` builds the hook first asks a
  **warm embedder daemon** (`kimetsu brain embed-daemon`, one per user,
  started/pre-warmed by `kimetsu brain warm` on `SessionStart`) for
  *semantic* retrieval over a local socket; if the daemon is unreachable
  within a tight budget (300ms) it falls back to lexical FTS for that turn
  and spawns the daemon for next time, so the prompt is never blocked. The
  daemon holds the ONNX model in memory once (no per-prompt cold load) and
  serves hybrid semantic retrieval with an absolute cosine floor, finished
  by a cross-encoder rerank of a 6-capsule pool (`ms-marco-tinybert-l-2-v2`
  by default, paired with the `jina-v2-base-code` embedder — both chosen by
  benchmark; see "Retrieval models & benchmarking" below). Toggles:
  `[embedder] daemon` / `warm_on_start` / `reranker`, or
  `KIMETSU_EMBED_DAEMON=0`.
- **`Stop` → `kimetsu brain stop-hook`** fires when the host supports a
  stop event. It walks the transcript, counts `kimetsu_brain_record`
  calls, and prints a one-line post-turn banner — either confirming how
  many lessons were captured or nudging the model to record one after a
  non-trivial, un-captured session.
- **`SessionEnd` → `kimetsu brain session-end-hook`** runs the optional
  credentialed distiller when the host exposes SessionEnd. Claude Code uses
  this event; Codex uses its supported `Stop` event with `--distill-on-stop`
  for the same deterministic harvest path.

These are plain CLI subcommands, so the same pattern works under any
harness that can run a command on a prompt, stop, session-end, or tool
event. The Codex installer wires prompt-time context, stop summaries,
Stop-time distilling, proactive tool hooks, and a
`kimetsu-memory-harvester` custom agent; the Claude Code installer wires
the same flow through `.claude/settings.json` and its subagent file.

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
(surfaced ids, last injection, recent commands) lives OUT of the repo, under
`~/.kimetsu/cache/<project-hash>/proactive/<session_id>.json`, and is GC'd
after 7 days — keeping `.kimetsu/` itself lean (just `brain.db` + `project.toml`).

Proactive hooks install by default with `kimetsu plugin install`; pass
`--no-proactive` (or `proactive:false` to `kimetsu_plugin_install`) to
skip `PreToolUse` / `PostToolUse` and keep only prompt-time context plus any
supported stop hook.

---

## 7a. Kimetsu Remote (beta)

Everything above assumes a **local** brain — one `.kimetsu/brain.db` next to your
checkout, reached over stdio MCP. Kimetsu Remote runs the brain on a **server**
and exposes it over **HTTP MCP**, so the identity is the **repository**, not a
local directory: any checkout of the same repo — on any machine, or a teammate's
— hits the same brain, with no local files required.

> **Beta** — under active testing; the `kimetsu-remote` server is a **separate
> package** (`npm i -g kimetsu-remote` or `cargo install kimetsu-remote
> --features embeddings`), not installed with the `kimetsu` CLI.

**The server.** `kimetsu-remote serve --data <dir> --token <secret>` hosts one
brain per repo under `<dir>/<repo-id>/`, keyed by a sanitized id the client sends
in the URL (`POST /mcp/<repo-id>`). It reuses the same transport-agnostic tool
dispatch as the stdio server, filtered to the **pure-DB, agent-facing subset**
(context, record, search, insights, curation) — the tools that need no checkout.
Per-repo SQLite + WAL gives concurrent reads; writes serialize through each
repo's lock; cross-repo is fully parallel.

**Auth + hardening.** Bearer tokens (global or per-repo, constant-time compared);
optional per-token rate limiting (`--rate-limit <req/min>` → `429`); a structured
per-request log and an aggregate Prometheus `GET /metrics` (no repo labels — it's
unauthenticated); plain HTTP by default (terminate TLS at a reverse proxy) or
in-process HTTPS with `--features tls` + `--tls-cert`/`--tls-key`.

**Client wiring.** `kimetsu plugin install <claude-code|openclaw> --remote <url>`
writes a remote MCP entry (`url` + `Authorization` header) instead of the local
stdio command, deriving the repo id from your git remote and referencing
`${KIMETSU_REMOTE_TOKEN}` so no secret hits disk.

**Optional extras.**
- **Shared org brain** (`--org-brain <dir>`): `global_user`-scoped memories are
  stored in one shared brain and merged into *every* repo's retrieval
  (cross-project team memory); `project`-scoped memories stay per-repo.
- **Server-side ingest** (`--repos-file` + `--checkout-dir`): the operator
  pre-registers repo-id → git URL; the server clones/refreshes a managed checkout
  and `kimetsu_brain_ingest_repo` indexes its files into the brain, so `context`
  retrieval includes **file capsules** remotely too. Clients can't trigger
  arbitrary clones; private repos use the server's own git auth.

### Retrieval models on the server

The remote server runs a **cross-encoder reranker** stage on every
`kimetsu_brain_context` call — the same stage the local daemon uses, but
operator-configured rather than per-repo.

**`--reranker <model>`** (default `jina-reranker-v1-tiny-en`, operator-level):
over-fetches a candidate pool of 6 capsules, runs the cross-encoder, drops noise
capsules below sigmoid score 0.30, and truncates to the caller's `max_capsules`.
`"off"` disables reranking. Any curated id or HuggingFace ONNX path is accepted
(same model registry as the local daemon).  The default was chosen by the 100-memory
benchmark — jina-tiny MRR 0.931 vs 0.914 for TinyBERT on the local bench; remote
has no hook-latency budget so the lightest reranker wins.

The **embedder** comes from per-repo config or `KIMETSU_BRAIN_EMBEDDER` (set before
seeding; reindex required after changes). The reranker is an operator flag and
cannot be overridden by a cloned repo's `project.toml` (untrusted on a server).

**Remote benchmark results** (100-case dataset, WITH jina-tiny reranker, production
floors active):

| embedder          | recall@2 | recall@4 |  MRR  | seq mean | rps  | peak RSS |
|-------------------|----------|----------|-------|----------|------|----------|
| jina-v2-base-code | 0.924    | 0.939    | 0.906 | 416ms    |  5.0 | 1198 MB  |
| bge-small-en-v1.5 | 0.929    | 0.939    | 0.909 | 700ms    |  3.8 |  697 MB  |

vs. pre-rerank baselines: jina-v2 was MRR 0.904, bge-small was MRR 0.901.

```bash
# Re-judge as your brain grows (one embedder per invocation):
kimetsu brain bench --remote --embedders jina-v2-base-code --dataset bench/dataset-100.json --out bench/results-100
kimetsu brain bench --remote --embedders bge-small-en-v1.5 --dataset bench/dataset-100.json --out bench/results-100
```

> **One embedder per invocation** — multi-embedder `--remote` runs seed later combos with the
> first embedder's vectors (process-global singleton). Kill stray `kimetsu-remote` processes
> between runs.

---


## 7b. Retrieval models & benchmarking (local)

The local retrieval stack is **embedder + cross-encoder reranker**, both
running warm inside the embed daemon. Defaults were chosen with
`kimetsu brain bench` — a benchmark seeded from REAL exported memories
(`bench/dataset-100.json`: 100 memories in confusable topic clusters, 210
cases — keyword, paraphrase, oblique, confusable, in-domain no-answer, open
multi-answer) that records expected-vs-obtained per case, latency, and RAM
per embedder × reranker combo (floors off — raw ranking quality):

| embedder          | reranker     | recall@2 | recall@4 |  MRR  | mean ms | peak RSS |
|-------------------|--------------|----------|----------|-------|---------|----------|
| jina-v2-base-code | jina-turbo   | 0.954    | 0.975    | 0.933 | 552     | 2.0 GB   |
| jina-v2-base-code | jina-tiny    | 0.949    | 0.975    | 0.931 | 414     | 2.0 GB   |
| jina-v2-base-code | minilm-l-4   | 0.949    | 0.959    | 0.927 | 372     | 2.3 GB   |
| **jina-v2-base-code** | **tinybert-l-2** | 0.914 | 0.949 | 0.914 | **132** | 1.5 GB |
| jina-v2-base-code | off          | 0.929    | 0.939    | 0.915 | 106     | 1.5 GB   |
| bge-small-en-v1.5 | off          | 0.919    | 0.934    | 0.905 | 446     | 359 MB   |

The default (**jina-v2-base-code + ms-marco-tinybert-l-2-v2**) is the
fastest reranked combo, within ~2% MRR of the grid best, and its rerank
stage fits the hook's 300ms budget. The jina-v2 embedder beats bge-small
across every reranker on this corpus (it recovers oblique, dev-phrased
queries bge never pools). Any reranker reliably beats none; the top three
rerankers are within noise of each other. Trade-off: ~1.5 GB resident
daemon (the lean-RAM option is `bge-small-en-v1.5`, ~360-525 MB, at
~1-3% lower MRR).

**Swapping models** (all local, takes effect after a daemon restart):

```bash
kimetsu config set embedder.model bge-small-en-v1.5      # or bge-m3, jina-v2-base-code
kimetsu config set embedder.reranker jina-reranker-v1-tiny-en   # or off, minilm, any HF org/repo ONNX
kimetsu brain reindex          # REQUIRED after an embedder change (vector dims differ)
kimetsu brain daemon stop      # next prompt/warm spawns a daemon with the new models
```

`KIMETSU_BRAIN_EMBEDDER` overrides the config per-process. Non-curated
rerankers load as user-defined ONNX from any HuggingFace repo.

**Re-judging as your brain grows** — the benchmark's value compounds:

```bash
kimetsu brain export bench/memories-export.json   # refresh the dataset source
kimetsu brain bench                               # full grid -> bench/results/summary.md
kimetsu brain eval                                # fixture-based quick check (recall@k, MRR)
```

Watch-item: the semantic floor (`broker.min_semantic_score`, 0.35) was
calibrated on bge-family cosine distributions; if you see over- or
under-filtering after an embedder change, re-tune it against
`kimetsu brain eval`.

## 8. The bridge

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

**`kimetsu doctor --selftest`** is the one-shot end-to-end proof: in a
throw-away temp project (it never touches your real workspace or user brain) it
records a sample memory and retrieves it back, printing
`✓ recorded a memory and retrieved it — the brain works` and exiting non-zero
if any step fails. Use it to confirm a fresh install actually works.

---

## 10. Configuration

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
min_semantic_score = 0.0      # >0 sets the embeddings-only relevance floor
budget_floor_tokens = 1500    # adaptive-budget floor (small tasks not starved)
budget_run_cap_tokens = 8000  # per-run ceiling on brain-injected tokens
compress_capsules = true      # v1.5: compress rendered capsule text (strips tags/context
                              # annotations, caps at 3 sentences); ranking unaffected
session_dedupe = true         # v1.5: skip capsules already injected this session

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
provider can differ — e.g. run the agent on **AWS Bedrock** (Anthropic models via
the InvokeModel API, SigV4-signed from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
(+ optional `AWS_SESSION_TOKEN`) and `AWS_REGION`) while the harvester stays on
direct Claude or OpenAI.

**Bidirectional config (off-switches).** Every optional feature is turn-off-able
in `project.toml` and honored at runtime with precedence
**env override > config > default**. Every field is `#[serde(default)]`, so a
partial `project.toml` loads cleanly — older files gain the new defaults on
upgrade. The toggles: `[embedder] enabled`, `[broker] ambient`,
`[kimetsu] use_user_brain`, plus the already-bidirectional `[learning]
auto_harvest`, `[learning.distiller] enabled`, and `[shell] redact_secrets`.
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

## 11. What kimetsu is NOT

- It's not a model. It runs through a host agent or a configured model provider
  (Anthropic API, Claude Code OAuth, OpenAI, or AWS Bedrock).
- It's not a sandbox. Tools run on the host machine.
- It's not an external vector DB. The brain is still a single SQLite file per
  project (FTS5 + optional cosine). On the embeddings build the semantic index
  is a usearch HNSW sidecar (`brain.usearch`) next to brain.db — no separate vector store,
  no service to run. Backups are still `cp brain.db` (and the brain also
  auto-backs-up to a `brain.db.bak-*` sidecar before any schema migration).

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
