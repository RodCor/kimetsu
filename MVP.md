# Kimetsu MVP

## Goal

Kimetsu v0.1 proves the local-first, brain-powered coding loop:

```text
repo/task
  -> ingest repo state
  -> retrieve relevant context and memory
  -> run a gated coding pipeline
  -> edit files
  -> verify with commands
  -> produce a traceable report
  -> propose useful memories
  -> reuse those memories on a later run
```

The falsifiable product claim for v0.1:

```text
The environment learning makes the next similar agent run better.
```

For MVP evaluation, "better" means at least one of:

```text
>=20% fewer tool calls on warm follow-up tasks
higher task success rate at similar tool/cost budget
fewer verification retries for similar tasks
```

## Non-Goals

v0.1 intentionally excludes:

```text
multi-agent orchestration
daily news crawling
graph database
vector database
GUI
cloud sync
remote sandboxing
provider-perfect token counting
automatic acceptance of inferred user memories
full web research benchmark support
SWE-bench Pro / PaperBench integration
```

## Principles

```text
No bloat.
Evidence-first memory.
Dynamic context loading.
Local-first.
Trace everything.
Own the core logic.
Dependencies only for infrastructure.
```

## Dependency Policy

Kimetsu's differentiating logic is owned code:

```text
agent loop
pipeline state machine
event model
memory scoring
context packing
tool policy
repo map format
PatchPlan validation
```

A dependency is allowed only when it provides boring infrastructure that is expensive or risky to implement incorrectly.

Allowed v0.1 dependencies:

```text
tokio                 async runtime and process management
serde, serde_json     serialization
toml                  project.toml parsing
clap                  CLI parsing
tracing               internal logs
rusqlite              SQLite projection, using bundled SQLite with FTS5
reqwest               provider HTTP calls
ulid                  sortable run/event IDs
time                  UTC timestamps as OffsetDateTime
ignore                gitignore-aware repo walking
regex                 redaction and output normalization
blake3                fast hashes and failure fingerprints
similar               diff rendering
```

Explicitly not in v0.1:

```text
agent frameworks
graph databases
vector databases
tree-sitter
embedding libraries
web crawlers
large plugin systems
binary-type detection crates for one small check
```

Before adding a new non-test dependency, add a short entry to a future `DEPENDENCIES.md`:

```text
crate
purpose
where used
why std/custom code is insufficient
removal cost
license
```

## Crate Layout

Start with four crates:

```text
crates/
  kimetsu-core/
    shared types, config, IDs, events, errors

  kimetsu-brain/
    SQLite projection, memory, repo ingestion, context broker

  kimetsu-agent/
    model provider, tool loop, coding pipeline

  kimetsu-cli/
    command interface
```

Do not split further until there is real implementation pressure.

## Core Types And Normalization

All repo paths stored in traces and SQLite are `RepoRelPath`.

`RepoRelPath` rules:

```text
UTF-8
NFC-normalized
forward slash separators
relative to canonical repo root
no absolute components
no `..`
no empty components
no drive prefixes
```

Filesystem conversion happens only at the boundary where a tool reads or writes a file.

Artifact references use relative paths under the current run directory:

```rust
ArtifactRef {
    path: String,
    bytes: u64,
    blake3: String,
}
```

Timestamps are UTC. User-facing local time conversion is a CLI display concern only.

## Canonical State

`trace.jsonl` is the source of truth.

SQLite is a derived query projection and can be rebuilt by replaying traces.

```text
.kimetsu/runs/<run_id>/trace.jsonl  canonical event log
.kimetsu/brain.db                   derived index and current projections
```

If `brain.db` and trace files disagree, the trace wins.

Durability policy:

```text
fsync on every stage.completed event
fsync on every tool.called, tool.completed, tool.failed event
fsync on every gate.passed and gate.failed event
fsync on every terminal run event (run.finished, run.failed, run.aborted)
no fsync on streaming model deltas
```

Per-event fsync would be too slow for streaming model output. Stage and tool boundaries are the durability points.

## Event Schema

Every event in `trace.jsonl` is one JSON object per line.

```rust
Event {
    event_id: Ulid,
    run_id: Ulid,
    ts: OffsetDateTime,
    parent_event_id: Option<Ulid>,
    kind: String,
    schema_version: u32,
    payload: serde_json::Value,
}
```

Use ULIDs instead of UUIDs for sortable replay.

Event kinds are hierarchical strings:

```text
run.started
run.finished
run.failed
run.aborted
stage.entered
stage.completed
model.requested
model.responded
tool.called
tool.completed
tool.failed
patch.plan.created
patch.plan.revised
gate.passed
gate.failed
memory.proposed
memory.accepted
memory.rejected
```

Core emits a closed known set in v0.1. Replay preserves unknown event kinds for forward compatibility, but unknown kinds are not indexed unless supported by the current projection schema.

Payload shape per kind is documented in the Tool Catalog and per-stage sections. v0.1 keeps `payload` typed as `serde_json::Value` for forward flexibility; per-kind payload types live in `kimetsu-core` as serde-derived structs validated at write time.

Terminal run events:

```text
run.finished
run.failed
run.aborted
```

## JSONL Write And Replay Semantics

Every event append writes exactly one compact JSON object followed by `\n`.

Replay rules:

```text
parse line by line
ignore empty lines
stop at the first invalid trailing line and warn
fail on invalid non-trailing lines
dedupe by event_id if a retry appended the same event twice
sort by ULID when rebuilding projections, then by file order as tie-breaker
```

Durability points flush and fsync the trace file after the event line is written.

## Event Payload Contracts

`payload` is validated against a per-kind Rust struct before writing.

Minimum v0.1 payloads:

```text
run.started
  { mode, task, project_id, repo_root, model, platform, kimetsu_version, config_hash }

run.finished
  { status: "success", final_report_path, total_cost_usd, total_tool_calls }

run.failed
  { category, message, hint?, failed_stage? }

run.aborted
  { reason, message? }

stage.entered
  { stage }

stage.completed
  { stage, summary, capsule_ids?: [Ulid] }

model.requested
  { stage, provider, model, request_artifact, tool_names, estimated_input_tokens }

model.responded
  { response_artifact, stop_reason, usage }

tool.called
  { stage, tool_call_id, tool, input_json }

tool.completed
  { tool_call_id, output_json, artifact_refs?, duration_ms }

tool.failed
  { tool_call_id, category, message, artifact_refs?, duration_ms }

patch.plan.created
  { patch_plan_id, artifact }

patch.plan.revised
  { patch_plan_id, revision_of, artifact }

gate.passed
  { kind, message, evidence_event_ids?: [Ulid] }

gate.failed
  { kind, message, evidence_event_ids?: [Ulid] }

memory.proposed
  { proposal_id, scope, kind, text, rationale, source_event_ids }

memory.accepted
  { proposal_id, memory_id }

memory.rejected
  { proposal_id, reason? }
```

## Projection Rules

SQLite projection updates only after stage commit events, terminal run events, and admin memory events.

Projection rebuild is supported by replaying traces:

```text
kimetsu_schema_version is stored in brain.db
schema version mismatch requires a rebuild before write operations
manual rebuild command: kimetsu brain rebuild
```

On startup, Kimetsu scans for runs without terminal events. For each orphaned run, it appends a synthetic `run.aborted` event with `reason: dirty_recovery`, fsyncs the trace, and then updates the projection. Existing trace events are never modified.

## Storage Layout

```text
.kimetsu/
  brain.db
  project.toml
  kimetsu.log
  runs/
    <run_id>/
      trace.jsonl
      final_report.md
      kimetsu.log
      patch_plans/
        <patch_plan_id>.json
      artifacts/
        <event_id>.stdout
        <event_id>.stderr
```

Long command outputs are stored as artifacts. Event payloads reference artifact paths relative to the run directory.

The model-visible command output is capped and summarized. Full output remains available in artifacts.

PatchPlan revisions are stored as separate JSON files under `patch_plans/`, one per `patch_plan_id`. The active plan is the most recent one referenced by `patch.plan.created` or `patch.plan.revised` events.

## SQLite Concurrency

v0.1 uses a single project writer lock.

Lock mechanism:

```text
.kimetsu/project.lock is created with atomic create_new
lock file payload: pid, command, run_id?, started_at
lock is removed on clean exit
```

If the lock exists, write commands fail fast and print the lock payload. Stale lock cleanup is manual in v0.1:

```bash
kimetsu lock clear --force
```

The command refuses to clear a lock unless `--force` is present.

Allowed concurrently:

```text
read-only commands
runs list/show
memory list
```

Serialized:

```text
run coding
memory accept/reject
repo ingest
projection rebuild
```

If another writer holds the lock, v0.1 fails fast with a clear message. There are no merge semantics in v0.1.

SQLite connections use WAL mode and a short busy timeout for read-only commands:

```text
PRAGMA journal_mode = WAL
PRAGMA busy_timeout = 5000
```

## Configuration

`project.toml` lives at `.kimetsu/project.toml` per repo. A user-level overlay at `~/.kimetsu/config.toml` is reserved for post-MVP.

Reference shape:

```toml
[kimetsu]
project_id     = "kimetsu-itself"
schema_version = 1

[model]
provider             = "anthropic"
model                = "claude-sonnet-4-5"
api_key_env          = "ANTHROPIC_API_KEY"
max_output_tokens    = 8192
temperature          = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

[broker.weights]
relevance  = 0.50
confidence = 0.20
freshness  = 0.20
scope      = 0.10

[broker.weights.localization]
relevance  = 0.70
confidence = 0.10
freshness  = 0.10
scope      = 0.10

[broker.weights.patch_plan]
relevance  = 0.40
confidence = 0.30
freshness  = 0.10
scope      = 0.20

[broker.weights.verification]
relevance  = 0.40
confidence = 0.10
freshness  = 0.40
scope      = 0.10

[broker.weights.review]
relevance  = 0.50
confidence = 0.20
freshness  = 0.20
scope      = 0.10

[shell]
default_timeout_secs = 60
max_timeout_secs     = 600
env_allowlist_extra  = ["RUSTFLAGS", "CARGO_HOME"]
redact_secrets       = true

[ingestion]
max_file_bytes  = 524288
extra_skip_dirs = []
max_total_files = 50000

[run]
max_total_tool_calls  = 60
max_total_model_turns = 30
max_total_cost_usd    = 5.0
```

API keys are never stored in `project.toml`. The `api_key_env` field names an environment variable; the value is read at runtime and never echoed to the trace.

Multi-repo project membership in v0.1 is by convention only: set the same `project_id` in each repo's `project.toml`. Cross-repo project memory sync is deferred.

## Brain Tables

Minimum projection tables and their meaningful columns and indexes:

```text
runs(run_id PK, project_id, task, started_at, ended_at,
     terminal_kind, model, total_cost_usd)

events(event_id PK, run_id FK, ts, kind, schema_version, payload_json)
  index: (run_id, ts)
  index: (kind, ts)

sources(source_id PK, kind, ref, hash, added_at)

memories(memory_id PK, scope, kind, text, normalized_text, confidence,
         source_event_id, provenance_snapshot_json, created_at,
         last_used_at, use_count)
  index: (scope, kind, normalized_text)

memory_proposals(proposal_id PK, run_id FK, scope, kind, text,
                 rationale, proposed_confidence,
                 source_event_ids_json, status, decided_at, decided_by)
  index: (status, run_id)

repo_files(repo_root, path, hash, size, mtime, language_guess, snippet)
  PK: (repo_root, path)
  index: (repo_root, language_guess)

repo_manifests(repo_root, manifest_path, manifest_kind,
               parsed_summary_json, hash, mtime)
  PK: (repo_root, manifest_path)

repo_files_fts(path, snippet, language_guess)
  virtual table: SQLite FTS5, content derived from repo_files

memories_fts(text, kind, scope)
  virtual table: SQLite FTS5, content derived from memories
```

Capsules are not persisted. Snippets stored in `repo_files.snippet` are the first 4 KB of file content used for BM25 relevance scoring.

FTS5 `bm25()` is the default relevance primitive. If FTS5 is unavailable at startup, `kimetsu init` fails with a clear dependency error instead of silently degrading retrieval.

## Memory Model

```rust
Memory {
    id: Ulid,
    scope: MemoryScope,
    kind: MemoryKind,
    text: String,
    normalized_text: String,
    confidence: f32,
    source_event_id: Ulid,
    provenance_snapshot: String,
    created_at: OffsetDateTime,
    last_used_at: Option<OffsetDateTime>,
    use_count: u32,
}
```

Memory scopes:

```text
global_user  applies across all projects
project      spans one or more repos under a named project
repo         tied to one canonical repo root
run          temporary, not persisted as accepted memory
```

Retrieval precedence:

```text
repo > project > global_user
```

Use "shadowed by", not "contradicted by". There is no contradiction or NLI detection in v0.1. A higher-scope memory is shadowed by a lower-scope memory of the same `kind` whose `normalized_text` overlaps.

Memory dedupe normalization:

```text
lowercase
collapse whitespace
strip trailing punctuation
NFC unicode normalization
no stemming
```

Confidence defaults:

```text
explicit memory_add        confidence = 1.0
inferred memory_proposal   confidence = 0.5 unless agent specifies
on accept of a proposal    confidence carried over from proposal
```

Runs are never garbage-collected in v0.1. Accepted memories also store a small `provenance_snapshot` so useful provenance survives future retention changes.

`run`-scoped memories are emitted as `memory.proposed{scope:run}` events, are visible to subsequent stages of the same run via the broker, and never enter `brain.db`.

## Memory Acceptance

Explicit user memories may be accepted directly.

Inferred memories must be proposed first.

Required commands:

```bash
kimetsu brain memory add --scope global_user "User prefers Rust for core infrastructure."
kimetsu brain memory list
kimetsu brain memory proposals
kimetsu brain memory accept <proposal_id>
kimetsu brain memory reject <proposal_id>
```

Reserve this command for post-MVP:

```bash
kimetsu brain memory invalidate <memory_id>
```

MemoryProposal schema:

```rust
MemoryProposal {
    proposal_id: Ulid,
    run_id: Ulid,
    proposed_at: OffsetDateTime,
    scope: MemoryScope,
    kind: MemoryKind,
    text: String,
    rationale: String,
    proposed_confidence: f32,
    source_event_ids: Vec<Ulid>,
    status: ProposalStatus,
}

ProposalStatus = Pending | Accepted | Rejected
```

When `memory accept` or `memory reject` runs outside a coding run, Kimetsu creates a real ULID-backed admin run:

```text
run.started{mode:"admin", task:"memory decision", project_id}
memory.accepted or memory.rejected
run.finished
```

The admin run has no PatchPlan and emits no verification events.

## Context Capsule Contract

The Context Broker returns compact, expandable capsules.

```rust
ContextCapsule {
    id: Ulid,
    kind: CapsuleKind,
    summary: String,
    token_estimate: u32,
    expansion_handle: String,
    provenance: Vec<ProvenanceRef>,
    confidence: f32,
    freshness: f32,
    relevance: f32,
    scope_weight: f32,
    score: f32,
}
```

Capsule kinds:

```text
task
memory
repo_file
repo_manifest
command
prior_run
failure_pattern
diff
gate_result
```

Expansion handles:

```text
memory:<memory_id>
file:<repo_relative_path>
run:<run_id>
event:<event_id>
artifact:<relative_path>
```

ProvenanceRef:

```rust
ProvenanceRef {
    source: ProvenanceSource,
    id: String,
    excerpt: Option<String>,
}

ProvenanceSource = Memory | Run | Event | RepoFile | Manifest | Artifact
```

`excerpt` is capped at 256 characters and is for human reading in `runs show` output, not for re-prompting.

Capsules are summaries first. Details are loaded only by explicit expansion.

## Capsule Scoring

Default score:

```text
score =
  0.50 * relevance +
  0.20 * confidence +
  0.20 * freshness +
  0.10 * scope_weight
```

Scope weights:

```text
run          1.00
repo         0.90
project      0.70
global_user  0.50
```

Freshness:

```text
freshness = exp(-age_days / half_life_days)
half_life_days = 30
```

Relevance is computed per capsule kind:

```text
repo_file        BM25 over (path tokens, manifest names, snippet)
repo_manifest    BM25 over (manifest_path, parsed_summary fields)
memory           BM25 over (text, kind keyword)
prior_run        cosine over bag-of-words on task strings
command          1.0 if exact match against detected commands, else 0
failure_pattern  BM25 over (text, last fingerprint substring)
diff             BM25 over (file paths in diff, hunk headers)
gate_result      1.0 if same gate failed earlier in this run, else 0
```

Relevance is normalized to 0..1 per kind before the weighted sum.

Stage-specific weight overrides come from `[broker.weights.<stage>]` in `project.toml`. Each returned capsule records its component scores so retrieval is explainable.

## Context Packing Algorithm

The broker is deterministic for a given input state.

Packing steps:

```text
1. compute candidate capsules for the requested stage
2. normalize relevance to 0..1 within each capsule kind
3. compute score using stage weights
4. remove shadowed duplicate memories
5. reserve budget for system instructions, task text, tool schemas, and response headroom
6. sort by score desc, then freshness desc, then id asc
7. include capsules until the remaining budget would be exceeded
8. emit a broker summary listing included and excluded high-score capsules
```

Default budget reservation:

```text
20% system and policy
15% tool schemas
15% response headroom
50% capsules
```

If the provider rejects the final request for size, the broker reruns with a 25% smaller capsule budget.

Capsule expansion is explicit. A stage may request expansion by `expansion_handle`; expanded content is still capped by the stage's remaining context budget.

## Stage-Aware Retrieval

Different pipeline stages receive different context.

Localization:

```text
repo file map
likely paths
matched text
symbols if available
similar prior failures
```

PatchPlan:

```text
repo conventions
accepted user/project/repo memories
related prior diffs
allowed target files
detected manifests
```

Implementation:

```text
active patch plan
full content of files_to_modify (capped at 64 KB per file)
snippets of files_to_read
repo conventions and command memories
recent diffs from prior runs touching the same files
```

Verification:

```text
detected commands
previous command failures
current diff
test/lint output summaries
failure-pattern memories
```

Review:

```text
active patch plan
final diff
changed files
acceptance criteria
gate decisions
```

MemoryProposal and FinalReport receive a trace summary only and make no broker requests for fresh capsules.

## Repo Ingestion

On every coding run, Kimetsu checks repo state before the pipeline starts.

Rules:

```text
canonicalize repo root using git root if available
respect .gitignore
ignore binaries (extension blocklist plus owned binary sniff)
ignore symlinks by default
ignore vendored/generated dirs:
  node_modules, target, dist, build, .next, vendor, .venv, __pycache__
cap indexed file content size, default 512 KB
store hash, size, mtime, language_guess, 4 KB snippet
detect manifests: Cargo.toml, package.json, pyproject.toml, go.mod
never index .env, .env.*, *.pem, *.key, id_rsa* or other obvious secret files
cap total scanned files at ingestion.max_total_files (default 50000)
```

Language guess rule:

```text
1. extension lookup against a static map
2. owned binary sniff: NUL byte in first 8 KB or common binary headers
3. fallback "unknown"
no tree-sitter in v0.1
```

Incremental policy:

```text
on every run, hash/mtime check and re-index changed files only
after agent edits, re-index modified files before MemoryProposal
```

## Worktree Ownership

Kimetsu must not confuse pre-existing user changes with run-owned changes.

At run start:

```text
capture git status --porcelain=v2 if git is available
capture hash/mtime for every file later read or modified
record the baseline in run.started or an artifact referenced by run.started
```

Run-owned changes are files whose content differs from the run-start baseline because of a Kimetsu tool call.

Strict diff gates operate on run-owned changes only. Pre-existing dirty files are allowed to exist, but Kimetsu may modify them only if:

```text
the file is declared in the active PatchPlan
the file was read during the run
apply_patch supplies the expected_hash from the latest read_file result
the current file hash still equals expected_hash
```

If a file changes after Kimetsu last read it, `apply_patch` fails with `file_changed_since_read`.

If git is unavailable, hash baselines still apply. Git-specific gates degrade to hash-based changed-file checks.

## PatchPlan Schema

```rust
PatchPlan {
    patch_plan_id: Ulid,
    run_id: Ulid,
    revision_of: Option<Ulid>,
    rationale: String,
    files_to_read: Vec<PathBuf>,
    files_to_modify: Vec<PathBuf>,
    files_to_create: Vec<PathBuf>,
    files_to_delete: Vec<PathBuf>,
    verification_commands: Vec<CommandSpec>,
    expected_outcome: String,
    risk_level: RiskLevel,
}
```

CommandSpec:

```rust
CommandSpec {
    program: String,
    args: Vec<String>,
    cwd_relative: PathBuf,
    timeout_secs: Option<u32>,
    expected_exit: i32,
}
```

`cwd_relative` empty means repo root. `timeout_secs` `None` means use `[shell].default_timeout_secs`. `expected_exit` defaults to 0.

RiskLevel:

```text
Low      default
Medium   touches more than 5 files, or modifies test scaffolding
High     touches build/CI configuration, or any files_to_delete entry
```

`High` plans require `--allow-high-risk` on `kimetsu run coding`, otherwise the gate fails before Implementation.

Strict diff gate:

```text
set(modified files) is a subset of active_patch_plan.files_to_modify
set(created files)  is a subset of active_patch_plan.files_to_create
set(deleted files)  is a subset of active_patch_plan.files_to_delete
```

If the agent needs to edit, create, or delete another file, it must emit `patch.plan.revised` before making that change.

## Coding Pipeline

The coding pipeline is a state machine:

```text
Intake
  -> RepoScan
  -> ContextRetrieval
  -> Localization
  -> PatchPlan
  -> Implementation
  -> Verification
  -> Review
  -> MemoryProposal
  -> FinalReport
```

Every stage emits:

```text
stage.entered
stage.completed or run.failed/run.aborted
```

Per-stage budgets:

```text
localization      max 4 model turns, 15 tool calls
patch_plan        max 3 model turns,  5 tool calls
implementation    max 8 model turns, 20 tool calls
verification      max 4 model turns, 10 tool calls per attempt (3 attempts)
review            max 2 model turns,  4 tool calls
memory_proposal   max 1 model turn,   0 tool calls
final_report      max 1 model turn,   0 tool calls
```

Run-level budgets in `[run]` are enforced in addition to stage budgets. Whichever caps first wins.

Stage outputs that affect control flow are structured JSON. Invalid JSON or missing required fields triggers one repair prompt. A second invalid response fails the run with `run.failed{category:"Provider", failed_stage:<stage>}`.

`final_report.md` format:

```text
# <task line>

## Summary
<short paragraph>

## Changed files
<bulleted list of paths and op>

## Verification
<command and result per gate>

## Memory proposals
<list of proposal_id, scope, kind, text>

## Known risks
<list>

## Trace
run_id: <run_id>
trace path: .kimetsu/runs/<run_id>/trace.jsonl

Generated by kimetsu <version>
```

Required final report sections are enforced by the FinalReport stage; missing sections cause `gate.failed{kind:report}`.

## Verification Gates

v0.1 gates:

```text
git status before/after
strict diff gate
format command if detected
lint command if detected
test command if detected
review of final diff
high-risk patch plan guard
cost ceiling
```

The "review of final diff" gate runs a Review-stage agent turn over the final diff and the PatchPlan's `expected_outcome`. The agent must output `gate.passed{kind:review}` with a one-line approval. Otherwise the stage emits `gate.failed{kind:review}` with the agent's stated reason.

There is no separate "no unrelated file changes" gate; that is enforced by the strict diff gate.

Simple command detection:

```text
Cargo.toml      cargo test
package.json    npm test / pnpm test / bun test
pyproject.toml  pytest
go.mod          go test ./...
```

## Failure Loop

Retry budget:

```text
2 repair attempts after the first failed verification (3 attempts total)
```

On retry, the agent sees:

```text
current patch plan
current diff
failed command
exit code
trimmed output summary
new/changed files since the previous attempt
relevant failure memories
last hypothesis
```

The "files changed since the previous attempt" baseline is captured by snapshotting `git status --porcelain=v2` at the entry of each Verification attempt. The next retry diffs against that snapshot, not HEAD.

Stop conditions:

```text
verification passes
retry budget exhausted
same failure fingerprint appears in two consecutive attempts
diff touches files outside PatchPlan
command violates policy
context/model/tool budget exceeded
cost ceiling exceeded
```

"Same failure twice" is consecutive, not lifetime: attempt N and N+1 must share a fingerprint to trigger this stop.

Failure fingerprint:

```text
fingerprint = blake3(
  exit_code,
  normalized_command_template,
  normalized_first_error_line
)
```

Normalization:

```text
lowercase
collapse whitespace
strip temp paths
strip absolute repo path
strip line/column numbers
strip timestamps
```

## Tool Catalog

The agent calls tools by name with JSON inputs. Outputs are JSON. Tool I/O is part of the spec, not implementation detail.

`read_file`:

```text
input  { path: RepoRelPath, max_bytes?: u32 }
output { path, content, truncated, hash, size }
errors path_outside_repo, not_found, binary, too_large
```

`search_files`:

```text
input  { pattern: String, glob?: String, max_results?: u32 }
output { matches: [{ path, line, column, excerpt }] }
errors invalid_regex
```

`list_files`:

```text
input  { dir: RepoRelPath, depth?: u32, glob?: String }
output { entries: [{ path, kind, size, mtime }] }
errors path_outside_repo
```

`shell_command`:

```text
input  CommandSpec
output {
  exit_code,
  stdout_summary,        capped at 64 KB
  stderr_summary,        capped at 64 KB
  stdout_artifact,       relative path under runs/<run_id>/artifacts
  stderr_artifact,
  duration_ms,
  timed_out: bool,
  killed_for_policy: bool
}
errors policy_violation, timeout_unset, cwd_outside_repo
```

`git_status`:

```text
input  {}
output { porcelain_v2: String, branch: String, ahead: u32, behind: u32 }
```

`git_diff`:

```text
input  { staged?: bool, paths?: [RepoRelPath] }
output { unified_diff: String, truncated: bool, full_artifact?: relative_path }
```

Output capped at 64 KB; truncation marker `<truncated, see artifact:<event_id>>` references the full artifact.

`apply_patch`:

```text
input {
  changes: [
    {
      path: RepoRelPath,
      op: "create"|"modify"|"delete",
      content?: String,
      expected_hash?: String
    }
  ]
}
output { applied_files: [RepoRelPath], unified_diff: String }
errors path_outside_repo, op_not_in_patch_plan, file_already_exists,
       file_missing, file_changed_since_read, expected_hash_required
```

`modify` and `delete` require `expected_hash`. `create` requires the file to not exist.

v0.1 uses whole-file replacement. There is no fuzzy unified-diff applier. Diffs visible to the model are computed by Kimetsu via `similar`.

All tool calls and results are written to trace as `tool.called`, `tool.completed`, or `tool.failed` events. Outputs that exceed the model-visible cap are stored as artifacts under the run directory.

## Instruction Boundary And Prompt Injection

Kimetsu treats external content as data, never as instructions.

Untrusted content includes:

```text
repo files
issue text
command output
test failure output
future web pages
future papers/news sources
```

Rules:

```text
system policy and tool policy are enforced outside the model
untrusted content is wrapped in data blocks with provenance
untrusted content cannot grant permissions or override PatchPlan/gates
tool calls are validated by Rust code regardless of model wording
network, shell, file write, and git restrictions are never prompt-only
```

If untrusted content asks Kimetsu to ignore rules, reveal secrets, alter policy, or run unrelated commands, the agent should summarize it as suspicious evidence and continue under the existing policy.

## Shell Tool Policy

Shell execution is allowed only under policy.

v0.1 command execution is not a security sandbox. It is a policy-checked local process runner intended for user-owned repos. Real filesystem and network isolation are deferred.

```text
cwd must be inside canonical repo root
commands must be non-interactive
timeout is required (default 60s, max 600s)
kill process tree on timeout
stdout/stderr are captured as artifacts
model-visible output is capped at 64 KB per stream
environment is allowlisted, not inherited wholesale
network disabled by default
writes allowed only inside repo root
recursive delete/move blocked
git push/commit/tag/reset --hard blocked
output is decoded as UTF-8 with replacement on invalid bytes
```

Process execution:

```text
v0.1 does not execute raw shell strings.
CommandSpec.program is spawned directly with CommandSpec.args.
Shell built-ins are unsupported unless invoked through an explicit program
that passes policy validation.
```

The resolved executable path is recorded in the `shell_command` tool result for reproducibility.

Process-tree kill:

```text
Unix:    spawn with setsid; kill via killpg(SIGTERM) then SIGKILL after 5s
Windows: spawn with CREATE_NEW_PROCESS_GROUP; kill via taskkill /T /F
```

Network enforcement in v0.1 is policy-only: refuse common network binaries by name, including:

```text
curl
wget
nc
netcat
ssh
scp
sftp
ftp
rsync
```

Real sandboxed network enforcement is post-MVP.

Secret redaction (controlled by `[shell].redact_secrets`, default `true`):

```text
regex applied to stdout and stderr before persisting artifacts
                                       and before model-visible cap
same redaction applied to tool input/output payloads before trace write
default pattern: (?i)(api[_-]?key|secret|password|token|bearer)[\s:=]+\S+
matches replaced with <redacted>
--no-redact CLI flag disables for the current run; documented as best-effort
```

## Model Provider

v0.1 implements one provider first, but the trait must not be provider-shaped.

```rust
trait ModelProvider {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream>;
    fn estimate_tokens(&self, text: &str) -> u32;
}
```

Kimetsu-owned request shape:

```text
messages
tools
tool_choice
max_output_tokens
temperature
metadata
```

ModelStream events:

```rust
ModelStreamEvent =
  | TextDelta(String)
  | ToolUseStart  { tool_call_id, tool, partial_input: Value }
  | ToolUseDelta  { tool_call_id, input_delta: String }
  | ToolUseEnd    { tool_call_id }
  | TurnEnd       { stop_reason, usage: TokenUsage }

TokenUsage { input_tokens, output_tokens, cost_usd }
```

## Model Transcript Storage

Model requests and responses are not inlined into `trace.jsonl`.

Artifacts:

```text
.kimetsu/runs/<run_id>/artifacts/<event_id>.model_request.json
.kimetsu/runs/<run_id>/artifacts/<event_id>.model_response.json
```

`model.requested.payload.request_artifact` points to the request artifact. `model.responded.payload.response_artifact` points to the response artifact.

The trace payload stores:

```text
provider
model
stage
tool names
token estimates
artifact refs
```

Full model transcripts are local artifacts for debugging and replay. They are redacted with the same best-effort redaction policy as shell output before writing.

`cost_usd` is computed from a per-provider price table baked into the adapter and accumulated against `[run].max_total_cost_usd`. Overrun emits `gate.failed{kind:cost}` and ends the run with `run.aborted`.

Provider adapters translate from Kimetsu format into provider-specific APIs.

Token counting is advisory in v0.1. If a provider rejects a request because the estimate was wrong, the agent retries with the broker's budget reduced by 25%, up to 2 attempts, and emits an event explaining the shrink.

Provider-level retries:

```text
HTTP 429 / 5xx     retry up to 3 times with exponential backoff (1s, 4s, 16s)
network error      retry up to 3 times with the same backoff
HTTP 4xx other     do not retry; emit run.failed{kind:provider}
```

## Authentication and Secrets

API keys are read from environment variables only. The variable name is given by `[model].api_key_env` in `project.toml`.

```text
ANTHROPIC_API_KEY (default for the anthropic provider)
```

Behavior:

```text
api_key_env unset and a stage needs the model
  -> gate.failed{kind:auth}
  -> run.failed with a clean error and the env var name to set
api_key_env set
  -> the value is read once at run start
  -> the value is never written to project.toml
  -> the value is never written to trace.jsonl or artifacts
```

`kimetsu init` warns if the configured `api_key_env` is unset, but does not fail.

The redaction regex (see Shell Tool Policy) does not depend on the API key value being known and runs over all shell output regardless.

## Kimetsu Init Flow

`kimetsu init` is idempotent.

Steps:

```text
1. detect git root via `git rev-parse --show-toplevel`, else use CWD
2. create .kimetsu/ if missing
3. write default project.toml if missing (never overwrite an existing file)
4. initialize brain.db with the current schema_version if missing
5. check that the env var named by [model].api_key_env is set; warn if not
6. optional: send a one-token smoke request to the configured provider
7. print a summary: project_id, repo_root, brain.db path, model, key status
```

`kimetsu init --force` rewrites `project.toml` to defaults. Existing `brain.db` and `runs/` are never deleted by `init`.

## Logging and Observability

`tracing` is used for Kimetsu-internal logs. These are distinct from `trace.jsonl`.

Destinations:

```text
stderr                                    human-readable, default level info
.kimetsu/kimetsu.log                      project-level log, rotated weekly
.kimetsu/runs/<run_id>/kimetsu.log        per-run mirror, lifetime of the run
```

Levels:

```text
KIMETSU_LOG=error|warn|info|debug|trace
RUST_LOG honored as fallback
```

Trace events go to `trace.jsonl`. Internal logs go to `kimetsu.log`. The two never share a sink.

## Error Model

User-facing errors are short, single-line messages with a category and a hint. `--debug` prints stack traces.

Categories:

```text
Config     bad project.toml, missing required key
Provider   model provider auth, rate limit, network
Repo       not a git repo, path outside repo, ingestion failure
Brain      brain.db corruption or schema mismatch
Tool       tool input validation, policy violation
Gate       verification or risk gate failure
Bug        unexpected internal error; user is asked to report
```

User-facing errors include `run_id` when applicable. The full error chain is in `kimetsu.log` and the matching trace events.

## Telemetry and Privacy

Kimetsu makes no network calls except to the configured model provider.

```text
no analytics
no error reporting
no update checks
no usage pings
```

The trace and brain stay on the user's machine. Memory proposals never leave unless the user shares them.

## Versioning and Upgrade

Two version numbers govern compatibility:

```text
kimetsu_schema_version   stored in brain.db, governs projection schema
Event.schema_version     per event, governs payload shape
```

Compatibility rules for v0.1:

```text
brain.db schema mismatch        requires `kimetsu brain rebuild` before write
trace.jsonl unknown kind        skipped during projection with a warning
trace.jsonl unknown payload v   skipped during projection with a warning
```

Memory format migration is deferred. v0.1 does not modify accepted memories on upgrade; it only refuses to write if the schema does not match.

## Resource Limits

```text
max_total_tool_calls    [run] in project.toml
max_total_model_turns   [run] in project.toml
max_total_cost_usd      [run] in project.toml
max_total_files         [ingestion] in project.toml (default 50000)
shell stdout/stderr     capped at 256 MB to disk per call
shell model-visible     capped at 64 KB per stream per call
file index snippet      first 4 KB of file content
read_file response      max_bytes parameter, default 64 KB
```

If shell stdout or stderr reaches the disk cap, Kimetsu terminates the process tree, emits `gate.failed{kind:output_size}`, and ends the run cleanly with `run.aborted`.

Other overruns emit `gate.failed{kind:budget|cost|file_count|output_size}` and end the run cleanly with `run.aborted`.

## Run Abort

`kimetsu run abort <run_id>` writes a sentinel file `.kimetsu/runs/<run_id>/.abort`.

The agent loop checks for the sentinel at every stage boundary and at every tool boundary. On detection it:

```text
emits run.aborted with reason: user_abort
flushes pending trace writes with fsync
releases the project writer lock
exits cleanly
```

If the harness crashes mid-run without writing a terminal event, startup recovery scans for runs with no `run.finished/failed/aborted` event and writes a synthetic `run.aborted{reason:dirty_recovery}` so the projection sees a terminal event. The original trace is never modified except to append this terminal event.

## CLI Surface

Required v0.1 commands:

```bash
kimetsu init [--force]

kimetsu config show
kimetsu config edit

kimetsu brain ingest-repo .
kimetsu brain memory add --scope global_user "User prefers Rust for core infrastructure."
kimetsu brain memory list
kimetsu brain memory proposals
kimetsu brain memory accept <proposal_id>
kimetsu brain memory reject <proposal_id>
kimetsu brain rebuild
kimetsu brain stats

kimetsu run coding --repo . "Fix the failing tests" \
  [--dry-run] [--allow-high-risk] [--no-redact] [--debug]
kimetsu run abort <run_id>

kimetsu runs list
kimetsu runs show <run_id>

kimetsu lock clear --force
```

`runs show <run_id>` includes trace/event details. There is no separate `runs trace` command in v0.1.

`run coding --dry-run` runs Intake through PatchPlan and stops; the PatchPlan is reported but no Implementation occurs.

`brain stats` reports memory counts by scope and kind, run counts, total tool calls, and total tracked cost.

## Implementation Phases

Phase 0: Spec and skeleton

```text
Cargo workspace
crate boundaries
config structs
dependency ledger placeholder
basic CLI shell
```

Done when:

```text
kimetsu --help
kimetsu init --help
all crates compile with empty implementations
```

Phase 1: Project state and event log

```text
init flow
storage layout
ULID events
trace append/replay
project writer lock
projection schema
```

Done when:

```text
kimetsu init
kimetsu runs list
kimetsu runs show <run_id>
kimetsu brain rebuild
```

work against synthetic runs.

Phase 2: Brain basics

```text
repo ingestion
memory add/list/proposals/accept/reject
basic repo file search
capsule scoring
context packing
```

Done when a query can return deterministic context capsules with provenance and scores.

Phase 3: Tools and policy

```text
read_file
search_files
list_files
shell_command
git_status
git_diff
apply_patch
path validation
worktree ownership
shell policy
artifact capture
```

Done when tool calls can be executed from a test harness and every call writes valid trace events.

Phase 4: Model provider and agent loop

```text
provider adapter
stream parser
tool call loop
model transcript artifacts
budget enforcement
structured stage output validation
```

Done when a model can read a file, request a patch, run a command, and stop under budget.

Phase 5: Coding pipeline

```text
state machine
PatchPlan creation/revision
strict diff gate
verification gates
failure loop
review gate
final report
memory proposal stage
```

Done when:

```text
kimetsu run coding --repo . "Fix the failing tests"
```

can complete on a controlled fixture repo.

Phase 6: MVP benchmark

```text
Kimetsu Bench fixtures
brain_off / brain_on_cold / brain_on_warm modes
paired seed/follow-up tasks
metric report
```

Done when the benchmark can produce the brain-on vs brain-off comparison for at least 12 curated tasks.

## Benchmark Plan

The internal benchmark is the primary v0.1 benchmark.

Modes:

```text
brain_off
brain_on_cold
brain_on_warm
```

Definitions:

```text
cold = empty brain except current repo ingestion
warm = pre-seeded with relevant memories from a paired seed task
```

Benchmark structure:

```text
5 Rust coding tasks
3 JS/TS coding tasks
2 memory reuse tasks
2 context-loading tasks
```

Use hand-curated pairs:

```text
task_a_seed
task_b_followup
```

Compare `task_b_followup` cold vs. warm after `task_a_seed` memory.

Per-task ground truth is hand-labeled at curation time. Each benchmark task ships with:

```text
expected_outcome   short prose description
success_check      CommandSpec; expected exit 0 means success
relevant_expansion_handles   labeled at curation; used to compute irrelevant_context_loaded
```

Metrics:

```text
success
tool calls
verification attempts
time
tokens/cost
irrelevant context loaded
accepted memories used
files changed
unrelated edits
```

Phase 6 starts with a broker-only benchmark slice:

```text
kimetsu bench run --repo .
```

This benchmark is deterministic by default. It creates temporary fixture
repos for 12 curated seed/follow-up pairs, runs `brain_off`,
`brain_on_cold`, and `brain_on_warm`, then writes:

```text
.kimetsu/bench/<bench_run_id>/report.md
.kimetsu/bench/<bench_run_id>/results.json
.kimetsu/bench/<bench_run_id>/artifacts/<task_id>/<mode>/trace.jsonl
.kimetsu/bench/<bench_run_id>/artifacts/<task_id>/<mode>/patch_plan.json
```

Slice 6 measured broker-only context and memory behavior. Slice 7 adds
deterministic dry-run PatchPlan traces for `brain_on_cold` and
`brain_on_warm`; model calls are disabled during this benchmark path so it
cannot depend on local API key state.

The report measures:

```text
success as relevant signal loaded
relevant file capsules loaded
accepted memories used
context loads
irrelevant context loaded
dry-run trace event count
per-stage time profile from trace stage events
model turns and model skips
tool calls
verification attempts
planned relevant files
unrelated planned files
```

Implementation edits, verification command execution, and model-backed cost
metrics are added when the implementation loop is enabled for benchmark
fixtures.

External benchmark order after v0.1:

```text
Terminal-Bench subset
LongMemEval-inspired memory tests
BrowseComp subset for research
SWE-bench Pro
PaperBench
```

## Known Risks And Defaults

Project bootstrap:

```text
.kimetsu/project.toml lives at the project root.
Multi-repo project setup is deferred.
Project membership is by shared project_id only.
```

Provider token mismatch:

```text
retry once with smaller context
emit event with original estimate and provider error
```

API key handling:

```text
keys are read from env only
keys never appear in project.toml, trace, or artifacts
init warns on missing key but does not fail
```

Self-logging:

```text
Kimetsu process logs use tracing to stderr and kimetsu.log.
Run trace events go to trace.jsonl.
Keep these separate.
```

Memory quality:

```text
prefer operational facts over vague summaries
inferred memories remain proposals
accepted memories require provenance
```

Cost runaway:

```text
max_total_cost_usd is enforced after every model.responded event.
Overrun aborts the run with run.aborted{kind:cost}.
```

Secret leakage in trace:

```text
shell stdout/stderr is regex-redacted by default before artifact write.
The redaction is best-effort; users with stricter needs should audit
their commands and consider running with redaction left on.
```

Cross-platform paths:

```text
paths in trace and brain.db are forward-slash UTF-8, NFC-normalized.
Native conversion happens at filesystem boundaries.
Paths with .. or absolute components are rejected at the repo boundary.
Case sensitivity follows the underlying filesystem.
```

Process execution:

```text
commands are spawned directly as program + args.
No raw shell string execution in v0.1.
The resolved executable is recorded in the tool result.
```

## Deferred

```text
multi-repo project bootstrap
real network sandboxing
provider-perfect token counting
memory invalidate command
trace compaction
run garbage collection
graph entities / claims / edges
capsule persistence
parallel project writers
remote sandboxing
news/paper ingestion pipeline
GUI
cloud sync
user-level config overlay (~/.kimetsu/config.toml)
embedding-based capsule relevance
tree-sitter symbol indexing
```
