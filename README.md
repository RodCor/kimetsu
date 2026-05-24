# Kimetsu

Kimetsu is an evidence-first AI coding and research harness written in Rust.
It has two separate surfaces:

- `kimetsu chat`: the user-facing terminal coding assistant. It runs against
  your workspace directly and does not require Harbor.
- `kimetsu bridge`: the cross-harness extension layer. It imports skills from
  Codex, Claude Code, Agents, and Kimetsu homes into `.kimetsu/extensions`,
  exports them back to other harnesses, and exposes the same controls through a
  local MCP sidecar.
- `kimetsu-harbor-agent`: the Terminal-Bench / Harbor adapter used only for
  benchmark runs.

The shared agent runtime provides tool use, cost accounting, project memory,
verification loops, and provider integration. Harbor is not part of the product
chat or bridge path.

## Current Status

- Rust workspace with six crates:
  - `kimetsu-cli`: top-level `kimetsu` binary.
  - `kimetsu-chat`: interactive terminal UI and REPL transport.
  - `kimetsu-agent`: protocol-neutral agent runtime and tool surface.
  - `kimetsu-brain`: project memory, retrieval, and curation.
  - `kimetsu-core`: shared config, events, ids, and path helpers.
  - `kimetsu-harbor-rs`: Terminal-Bench / Harbor transport adapter.
- `kimetsu chat` is the full user harness and works without Harbor.
- `kimetsu bridge` is the portability layer for skills and future extensions.
- `kimetsu plugin install claude` and `kimetsu plugin install codex` wire the
  current workspace to Kimetsu's MCP sidecar.
- Harbor remains isolated as benchmark glue.
- Latest local validation run: `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo test --workspace` passed. The workspace test run covered 134 unit
  tests across agent, brain, chat, CLI, core, and Harbor crates.

## Prerequisites

- Rust stable with Cargo.
- For model-backed chat: Claude Code credentials via
  `CLAUDE_CODE_OAUTH_TOKEN`.
- Optional for Terminal-Bench only: Python 3.10+, Harbor, and a Harbor
  execution backend such as Docker, Daytona, E2B, Modal, or Runloop.

## Quick Start

### Install

Pick whichever flavor matches your appetite:

```bash
# 1. Lean install — FTS-only retrieval, no model download (~30s).
cargo install kimetsu-cli

# 2. With local semantic retrieval — pulls fastembed-rs + ONNX
#    runtime; first-run downloads BGE-small (~67 MB).
cargo install kimetsu-cli --features embeddings

# 3. From source (anyone tracking `main` or a branch):
cargo install --path crates/kimetsu-cli [--features embeddings]
```

Once installed, `kimetsu` is on your PATH. Confirm it's healthy:

```bash
kimetsu --version
kimetsu doctor
```

Pre-built binaries for Linux / macOS / Windows ride on each
GitHub Release — drop one into `~/.local/bin` if you'd rather
skip the Rust toolchain.

### First chat

```bash
# Anywhere you have CLAUDE_CODE_OAUTH_TOKEN exported:
kimetsu chat --workspace .

# Or use the workspace .env file:
cp .env.example .env
$EDITOR .env       # set CLAUDE_CODE_OAUTH_TOKEN
kimetsu chat --workspace .
```

`.env` should contain:

```dotenv
CLAUDE_CODE_OAUTH_TOKEN=<your-token>
# Optional, only for /image with OpenAI image-capable models:
OPENAI_API_KEY=<your-openai-key>
```

Token resolution order: process environment first, workspace `.env` second.

### Pick a semantic model (when built with `--features embeddings`)

```bash
export KIMETSU_BRAIN_EMBEDDER=bge-small-en-v1.5   # default: 384 dim, English
export KIMETSU_BRAIN_EMBEDDER=bge-m3              # 1024 dim, multilingual, larger
export KIMETSU_BRAIN_EMBEDDER=jina-v2-base-code   # 768 dim, code-tuned
export KIMETSU_BRAIN_EMBEDDER=noop                # force off
kimetsu brain reindex                              # backfill embeddings on existing memories
```

## Bridge Quick Start

Use this path when Kimetsu should be the glue between Claude Code, Codex,
Agents, and its own chat harness.

Scan what is available:

```bash
cargo run -p kimetsu-cli -- bridge scan --workspace .
```

Import one discovered skill or all provider skills into Kimetsu-owned
extensions:

```bash
cargo run -p kimetsu-cli -- bridge import reviewer --workspace .
cargo run -p kimetsu-cli -- bridge sync --workspace .
```

Export a Kimetsu extension to another harness:

```bash
cargo run -p kimetsu-cli -- bridge export reviewer codex --workspace .
cargo run -p kimetsu-cli -- bridge export reviewer claude --workspace .
```

Install Kimetsu as a local MCP sidecar for host harnesses:

```bash
cargo run -p kimetsu-cli -- plugin install claude --mode optional --workspace .
cargo run -p kimetsu-cli -- plugin install codex --mode required --workspace .
```

The installers create workspace-local config only:

| command | writes |
|---------|--------|
| `kimetsu plugin install claude --mode <mode>` | `.claude/mcp.json`, `.claude/commands/kimetsu/bridge.md`, `.claude/commands/kimetsu/delegate.md`, `.claude/hooks/pre-turn.*`, `.claude/hooks/post-turn.*` |
| `kimetsu plugin install codex --mode <mode>` | `.codex/mcp.json`, `.codex/skills/kimetsu-bridge/SKILL.md`, `.codex/hooks/pre-turn.*`, `.codex/hooks/post-turn.*` |

Both MCP configs point at:

```bash
kimetsu mcp serve --workspace .
```

Mode defaults to `optional`, which recommends Kimetsu brain first and writes
soft-audit hooks. `required` writes stronger host instructions and hooks that
treat missing Kimetsu brain context as a setup blocker for non-trivial tasks;
benchmark wrappers can still add transcript-level enforcement.

That sidecar exposes Kimetsu brain context, benchmark playbooks, outcome
recording, and memory curation tools first, then bridge status, skill search,
import, export, sync, and plugin install tools to the host harness.

## Chat Usage

Start the terminal assistant:

```bash
cargo run -p kimetsu-cli -- chat --workspace .
```

Useful flags:

```bash
cargo run -p kimetsu-cli -- chat --workspace . --project .
cargo run -p kimetsu-cli -- chat --workspace . --max-cost-usd 1
cargo run -p kimetsu-cli -- chat --workspace . --plain
cargo run -p kimetsu-cli -- chat --workspace . --no-logo
```

Common slash commands inside chat:

```text
/
/help
/status
/context
/compact focus on current architecture decisions
/plan migrate the parser safely
/transcript last 5
/copy
/diff
/checkpoint before refactor
/undo
/new spike parser
/resume last
/export .kimetsu/chat/latest.md
/permissions read-only
/permissions auto
/model claude-opus-4-7
/effort high
/theme rich
/statusline on
/raw on
/keybindings
/cost
/goal refactor the parser safely
/strict on
/skills list
/skills sources
/skills search review
/skills use refactor
/skills install refactor
$refactor apply this workflow to src/parser.rs
! git status --short
/run cargo check --workspace
/run --terminal npm create vite@latest
/verify
/verify --terminal ./scripts/manual-check.ps1
/image chibi dragon mascot holding a terminal
/hooks list
/hooks run pre-turn
/mcp list
/mcp tools filesystem
/mcp call filesystem read_file {"path":"README.md"}
/bridge scan
/bridge import reviewer
/bridge export reviewer codex
/agents list
/agents run reviewer inspect the current diff
/agents start reviewer inspect auth and tests
/agents output agent-...
/tasks run cargo check --workspace
/tasks terminal npm run dev
/tasks list
/tasks output task-...
/tasks stop task-...
/review focus on tests
/security-review auth and shell execution
/simplify focus on duplication
/doctor
/debug-config
/clear
/quit
```

In an interactive terminal, pressing `/` opens a transient command palette
above the active prompt. It filters as you type, supports Up/Down selection,
Tab completion, and clears before running the command. When input is piped, `/`
still works as a line-based command.

The composer supports cursor movement, Home/End, Backspace/Delete, Up/Down
history, `Ctrl+R` history search, `Ctrl+L` redraw, `Ctrl+D` exit, `Ctrl+C`
clear-or-exit, `Ctrl+G` external editor handoff, and newline insertion with
Shift/Alt/Ctrl+Enter where the terminal reports those keys. Tab completes slash
commands, `@path` file mentions, and `$skill` mentions.

Environment toggles:

```bash
KIMETSU_MODEL=claude-opus-4-7
KIMETSU_PLAIN=1
NO_COLOR=1
KIMETSU_CLAUDE_PERSISTENT=0
```

By default `kimetsu chat` uses the richer terminal UI when stdout is an
interactive terminal. Redirected output and tests stay plain.

CLI-launched chat sessions are saved under `.kimetsu/chat/sessions`.
Checkpoints capture tracked-file diffs and `/undo` restores the last checkpoint
only when the current diff still matches that checkpoint, so Kimetsu refuses to
rewind over unrelated user edits.

`/image` is model-gated. It only runs for known OpenAI image-capable models and
requires `OPENAI_API_KEY`; Claude Code models keep the command disabled.

`/run` and `/verify` capture output by default. Use `--terminal` (`--tty` and
`--interactive` are aliases) when the command has prompts, nested terminal
steps, installers, dev servers, or a TUI. Kimetsu gives the command the real
terminal, waits for it to exit, then returns to chat. `/tasks terminal <command>`
does the same from the task surface; `/tasks run <command>` remains background
capture.

Prefix modes keep routine work fast: `! <command>` runs a captured shell command
and adds the result to the transcript, `@path` expands files into the next model
request, and `$skill <prompt>` loads a skill and applies it to the prompt.
`/plan` enables read-only planning mode until `/plan off`; `/compact` summarizes
the session into a durable one-turn summary. `/review`, `/security-review`, and
`/simplify` run focused read-only reviews of the current git diff.

Hooks, MCP, agents, and background tasks are active chat runtimes:

- Hooks run executable scripts from `.kimetsu/hooks`, `.claude/hooks`, or
  `.codex/hooks` for `session-start`, `session-end`, `pre-turn`, and
  `post-turn`.
- MCP loads stdio server configs from `.kimetsu/mcp.json` or
  `.claude/mcp.json`, then supports `/mcp tools` and `/mcp call`.
- Agents load Markdown or JSON definitions from `.kimetsu/agents`,
  `.claude/agents`, or `.codex/agents` and run with isolated system prompts.
- Tasks run local background commands and write output under
  `.kimetsu/chat/tasks`; running tasks are stopped when chat exits.

## Skills

Kimetsu supports Agent Skills, Codex skills, and Claude Code compatible skill
folders. A skill is the whole folder. `SKILL.md` is the required entrypoint,
but bundled files are part of the skill and are listed for on-demand use.

Example:

```text
.codex/skills/refactor/
|-- SKILL.md
|-- scripts/
|   `-- check.ps1
|-- references/
|   `-- guide.md
|-- assets/
|   `-- schema.json
`-- templates/
    `-- example.txt
```

Load skills at startup:

```bash
cargo run -p kimetsu-cli -- chat --workspace . --skill refactor
cargo run -p kimetsu-cli -- chat --workspace . --skill-dir ~/.codex/skills --skill refactor
cargo run -p kimetsu-cli -- chat --workspace . --skill ./.claude/skills/frontend-design
```

Kimetsu scans these workspace roots by default:

```text
.kimetsu/skills
.codex/skills
.claude/skills
```

The CLI also scans logged-in user tool homes by default:

```text
~/.kimetsu/skills
~/.codex/skills
~/.claude/skills
~/.agents/skills
~/.codex/plugins/cache/*/*/*/skills
~/.claude/plugins/cache/*/*/*/skills
~/.agents/plugins/cache/*/*/*/skills
```

Those plugin-cache roots are treated as provider marketplaces. Kimetsu only
loads `SKILL.md` when a skill is selected; bundled scripts, references, assets,
and templates remain on demand.

Useful discovery and import commands:

```bash
cargo run -p kimetsu-cli -- chat --workspace . --list-skill-sources
cargo run -p kimetsu-cli -- chat --workspace . --list-skills
cargo run -p kimetsu-cli -- chat --workspace . --search-skills refactor
cargo run -p kimetsu-cli -- chat --workspace . --install-skill refactor
```

Inside `kimetsu chat`, run `/skills` or `/skills select` in a real terminal to
open an interactive selector. Type to search, use arrow keys to navigate, press
Space or Enter to load/unload skills for the session, and press `i` to import
an external provider skill into `.kimetsu/skills`.

`--install-skill` copies the complete source bundle into
`.kimetsu/skills/<name>` and records `.kimetsu-skill-origin.json`, so an
installed Codex, Claude, Agents, or marketplace skill becomes a Kimetsu skill.
Use `--install-skill-force` to replace an existing import.

Use `--no-workspace-skills` or `--no-user-skills` to narrow discovery.

## Bridge And MCP Plugin

Kimetsu can run as a cross-harness bridge for Claude Code, Codex, Agents, and
Kimetsu. The bridge normalizes portable skill bundles into
`.kimetsu/extensions`, then exports those bundles back into another harness when
useful.

```bash
cargo run -p kimetsu-cli -- bridge scan --workspace .
cargo run -p kimetsu-cli -- bridge import reviewer --workspace .
cargo run -p kimetsu-cli -- bridge export reviewer codex --workspace .
cargo run -p kimetsu-cli -- bridge sync --workspace .
```

Install Kimetsu as a local MCP sidecar for a host harness:

```bash
cargo run -p kimetsu-cli -- plugin install claude --mode optional --workspace .
cargo run -p kimetsu-cli -- plugin install codex --mode required --workspace .
cargo run -p kimetsu-cli -- mcp serve --workspace .
```

`plugin install claude` writes a `.claude/mcp.json` entry plus Kimetsu command
prompts under `.claude/commands/kimetsu`. `plugin install codex` writes a
`.codex/mcp.json` entry plus a Codex-compatible `kimetsu-bridge` skill. The
MCP server exposes Kimetsu brain context, benchmark playbooks, outcome
recording, and memory curation tools first, then bridge status, skill
search/import/export/sync, and plugin install tools so Claude Code or Codex can
call Kimetsu as a live sidecar.

Use `--mode optional` when Kimetsu should be an available memory/brain helper.
Use `--mode required` when the installed host artifact should tell Codex or
Claude Code to load Kimetsu brain context before non-trivial work and to stop
for setup when Kimetsu is unavailable. Both modes install `pre-turn` and
`post-turn` hooks. The pre-turn hook calls `kimetsu brain context --json`; the
post-turn hook checks the per-session marker under `.kimetsu/hooks/usage/`.
Required hooks fail the turn on missing context, while optional hooks only warn.
Benchmark-grade enforcement can additionally inspect those markers or MCP
transcripts in a wrapper such as the Terminal-Bench Harbor adapter.

For Terminal-Bench harnesses, the main MCP entry point is
`kimetsu_benchmark_context`: pass the task text and dataset, then use the
returned `playbook_markdown` before broad exploration. After an attempt, call
`kimetsu_benchmark_record_outcome` with pass/fail/error status, commands,
pitfalls, and verification steps. This records exact attempts as
`memory_role=episodic`; add `generalized_memory` with
`memory_role=semantic_operator` or `anti_pattern` only for reusable tactics or
warnings that should be reviewed before becoming durable memory. For other
host tasks, call `kimetsu_brain_context` with the current task as `query` and
use the returned memory/repo/manifest capsules before planning or editing.
`kimetsu_brain_status`, `kimetsu_brain_memory_top`, and the proposal accept/reject/invalidate tools
expose Kimetsu's brain management loop without forcing Codex or Claude Code to
run the full Kimetsu agent.

Benchmark calls also accept `warm_policy`: `cold_brain` excludes accepted
memory capsules, `reactive_warm` leaves Kimetsu available without requiring
task memory up front, and `full_warm` is the pre-task playbook injection used
for required-mode comparisons.

Bridge commands are intentionally file-based. Imported skills keep their whole
bundle, not only `SKILL.md`, so scripts, references, templates, and assets stay
available after crossing harnesses. Existing target files are preserved unless
`--force` is passed.

## Project Memory

Initialize a Kimetsu project:

```bash
cargo run -p kimetsu-cli -- init
```

Run chat with memory retrieval enabled:

```bash
cargo run -p kimetsu-cli -- chat --workspace . --project .
```

Useful brain commands:

```bash
cargo run -p kimetsu-cli -- brain ingest-repo .
cargo run -p kimetsu-cli -- brain search "build failures"
cargo run -p kimetsu-cli -- brain context "where is chat configured?"
cargo run -p kimetsu-cli -- brain context "where is chat configured?" --json
cargo run -p kimetsu-cli -- brain memory list
cargo run -p kimetsu-cli -- brain memory proposals
cargo run -p kimetsu-cli -- brain memory review
cargo run -p kimetsu-cli -- brain memory top
```

Project memory is intended to make repeated work cheaper and more consistent.
The chat REPL keeps one brain session open for the conversation and injects
retrieved capsules into each model turn when `--project` is set.

## Development

Format, lint, and test:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Run the CLI help:

```bash
cargo run -p kimetsu-cli -- --help
cargo run -p kimetsu-cli -- chat --help
```

Run a non-chat coding pipeline:

```bash
cargo run -p kimetsu-cli -- run coding --repo . "summarize the workspace"
cargo run -p kimetsu-cli -- run coding --repo . --dry-run "plan a cleanup"
```

Run Kimetsu's internal bench harness:

```bash
cargo run -p kimetsu-cli -- bench run --limit 3
cargo run -p kimetsu-cli -- bench run --model-backed --limit 3 --max-cost-usd 5
```

## Harbor / Terminal-Bench

Harbor is benchmark-only. The user-facing chat harness does not import or
require it.

Build and smoke-test the Harbor adapter:

```bash
cargo build -p kimetsu-harbor-rs
python kimetsu_harbor/smoke_test.py
```

Build a release Harbor agent:

```bash
cargo build --release -p kimetsu-harbor-rs
```

Example real Terminal-Bench invocation:

```bash
PYTHONPATH="$(pwd)" \
KIMETSU_HARBOR_BIN="$(pwd)/target/release/kimetsu-harbor-agent" \
harbor run --dataset terminal-bench/terminal-bench-2 \
  --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
  -n 4
```

With brain enabled for a benchmark run:

```bash
PYTHONPATH="$(pwd)" \
KIMETSU_HARBOR_BIN="$(pwd)/target/release/kimetsu-harbor-agent" \
KIMETSU_HARBOR_PROJECT="$(pwd)" \
harbor run --dataset terminal-bench/terminal-bench-2 \
  --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
  -n 4
```

PowerShell:

```powershell
$env:PYTHONPATH = (Get-Location).Path
$env:KIMETSU_HARBOR_BIN = "$pwd\target\release\kimetsu-harbor-agent.exe"
harbor run `
  --dataset terminal-bench/terminal-bench-2 `
  --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent `
  -n 4
```

More Harbor setup details live in `kimetsu_harbor/README.md`.

## Metrics We Track

Kimetsu records metrics at several layers.

Chat turn metrics:

- Per-turn and cumulative cost.
- Model turns per user request.
- Tool calls per user request.
- Anthropic prompt-cache counters: `cache_write` and `cache_read`.
- Input and output tokens when reported by the provider.

Bench report metrics:

- Success rate by mode.
- Relevant signal rate.
- Accepted memories used.
- Context loads and irrelevant context loads.
- Trace events.
- Model turns and model skips.
- Tool calls.
- Verification attempts.
- Planned relevant files, unrelated planned files, and invalid planned files.
- Patch-plan quality score.
- Total and average duration.
- Stage timing summaries.
- Total model cost.

Brain and memory metrics:

- Memory proposal status: pending, accepted, rejected.
- Memory usefulness ratios.
- Invalidated memories.
- Retrieval context usage against token budget.

## Latest Recorded Benchmark Results

The most recent recorded Terminal-Bench slice in `docs/archive/MP-14-RESULTS.md`
uses `terminal-bench/terminal-bench-2`, a 16-task slice, `claude-opus-4-7`,
`-l 16 -n 2 -k 1`, and retry-on-5xx provider handling.

| run | wins/16 | mean reward | cost | cost per win |
|-----|--------:|------------:|-----:|-------------:|
| Bare Claude Code, MP-10b | 9 | 0.5625 | $22.24 | $2.47 |
| Kimetsu no-brain, MP-14d | 6 | 0.3750 | $2.12 | $0.35 |
| Kimetsu brain, MP-14d | 7 | 0.4375 | $1.36 | $0.19 |

Key takeaways from that run:

- Brain beat no-brain by one task: 7/16 vs 6/16, a +6.25 percentage point
  margin.
- Brain cost was 36% lower than no-brain on the same slice.
- Brain cost was 94% lower than bare Claude Code: $1.36 vs $22.24.
- Cost per win was about 13x lower than bare Claude Code: $0.19/win vs
  $2.47/win.
- Relative accuracy was 78% of bare Claude Code on the slice: 43.75% vs
  56.25%.
- No-brain stability was consistent across three recorded runs: MP-10,
  MP-13, and MP-14d all landed at 6/16.

The earlier MP-13g brain rerun is also useful because it isolates the
retry-on-5xx change:

| run | wins/16 | mean reward | cost |
|-----|--------:|------------:|-----:|
| Kimetsu brain, MP-13g | 6 | 0.3750 | $1.09 |

These numbers are not a full leaderboard claim. They are a recorded,
repeatable development slice used to compare Kimetsu modes and cost behavior.

## Performance Work Already Landed

Recent performance changes are summarized in `docs/V0.3.5-PERF.md`:

- Simple greetings and identity questions are answered locally with no model
  call or tool prompt.
- General non-workspace questions use a text-only model route with no tool
  catalog.
- Workspace agent turns use dynamic tool loading: start with read/inspect
  tools plus `load_tools`, then load edit, shell, background, image, or full
  profiles only when required.
- Chat auto-orientation runs only on the first model turn.
- `kimetsu chat` defaults Claude persistent mode on; set
  `KIMETSU_CLAUDE_PERSISTENT=0` to disable it.
- `kimetsu chat` reads workspace `.env` files for user-local credentials.
- `multi_read` is native Rust instead of shelling out for each file slice.
- Harbor `read_file` and `list_files` use workspace-safe filesystem helpers.
- Chat reuses one brain session instead of reopening project state per turn.
- Memory and manifest retrieval use FTS tables where available.
- Hot brain queries use cached prepared statements.
- Small shell summaries avoid unnecessary artifact writes.
- Chat disables trace fsync for faster interactive turns while pipeline runs
  keep durable fsync by default.

## Documentation Map

- `docs/KIMETSU-CHAT.md`: chat setup, terminal UI, credentials, and skills.
- `docs/archive/V0.3-PLAN.md`: chat product split and Harbor separation.
- `docs/V0.3.5-PERF.md`: latest performance work.
- `docs/archive/MP-14-RESULTS.md`: latest recorded Terminal-Bench comparison.
- `docs/archive/MP-13G-RESULTS.md`: retry-on-5xx brain rerun.
- `kimetsu_harbor/README.md`: Harbor adapter setup and benchmark runs.
- `docs/SWEBENCH.md`: SWE-bench integration plan.
