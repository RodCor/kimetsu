# Kimetsu Chat Setup

`kimetsu chat` is the user-facing Rust harness. It does not require Harbor;
Harbor is only the Terminal-Bench adapter.

## Token Setup

Kimetsu resolves `CLAUDE_CODE_OAUTH_TOKEN` in this order:

1. The current process environment.
2. The workspace `.env` file passed by `--workspace`.

For a local checkout:

```pwsh
Copy-Item .env.example .env
notepad .env
cargo run -p kimetsu-cli -- chat --workspace .
```

The `.env` entry should be:

```dotenv
CLAUDE_CODE_OAUTH_TOKEN=<your-token>
# Optional, only for /image with OpenAI image-capable models:
OPENAI_API_KEY=<your-openai-key>
```

`.env` is git-ignored; `.env.example` is the committed template. Each user
keeps their own token locally.

## Common Commands

```pwsh
cargo run -p kimetsu-cli -- chat --workspace .
cargo run -p kimetsu-cli -- chat --workspace . --project .
cargo run -p kimetsu-cli -- chat --workspace . --max-cost-usd 1
```

Set `KIMETSU_MODEL` in the process environment to change the default model.
Set `KIMETSU_CLAUDE_PERSISTENT=0` to disable persistent Claude subprocesses.

## Terminal UI

When stdout is an interactive terminal, `kimetsu chat` renders a lightweight
ANSI interface with the chibi dragon banner, session status, readable assistant
blocks, and turn/cache summaries. Redirected output and tests stay plain.

Use these flags when needed:

```pwsh
cargo run -p kimetsu-cli -- chat --workspace . --plain
cargo run -p kimetsu-cli -- chat --workspace . --no-logo
```

`NO_COLOR=1` or `KIMETSU_PLAIN=1` also disables the rich terminal renderer.
Inside chat, `/clear` drops the in-session transcript and redraws the banner;
`/logo` shows the dragon banner again.

In an interactive terminal, pressing `/` opens a transient command palette
above the active prompt. It filters as you type, supports Up/Down selection,
Tab completion, and clears itself before the command runs so it does not pollute
the active input line. In piped input or test mode, enter `/` as a line to print
the same list. `/help` is kept as an alias.

The composer supports left/right cursor movement, Home/End, Backspace/Delete,
Up/Down prompt history, `Ctrl+R` history search, `Ctrl+L` redraw, `Ctrl+D`
exit, `Ctrl+C` clear-or-exit, `Ctrl+G` external editor handoff, and
Shift/Alt/Ctrl+Enter newline insertion when the terminal reports those keys.
Use `@path` for file mentions and press Tab to complete paths; use `$skill` to
invoke or complete an available skill.

## Chat UX Commands

Kimetsu stores CLI-launched chat sessions under `.kimetsu/chat/sessions` and
can export transcripts as Markdown:

```text
/status
/context
/compact focus on current architecture decisions
/plan migrate the parser safely
/transcript last 5
/copy
/new spike parser
/resume
/resume last
/export .kimetsu/chat/latest.md
```

Workspace safety and review commands:

```text
/permissions read-only
/permissions auto
/permissions full
/theme rich
/statusline on
/raw on
/keybindings
/diff
/checkpoint before refactor
/undo
```

Project commands:

```text
/run cargo check --workspace
/run --terminal npm create vite@latest
/verify
/verify --terminal ./scripts/manual-check.ps1
/hooks list
/hooks run pre-turn
/mcp list
/mcp tools filesystem
/mcp call filesystem read_file {"path":"README.md"}
/bridge scan
/bridge import reviewer
/bridge export reviewer codex
/skills sources
/skills install refactor
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
```

`/verify` infers a project recipe when possible: Cargo projects use
`cargo test --workspace`, Node projects use `npm test`, Python projects use
`pytest`, and Makefile projects use `make test`.

`/run` and `/verify` capture command output by default. Add `--terminal`
(`--tty` or `--interactive` also work) when a command opens prompts, nested
menus, installers, dev servers, or a TUI. In terminal mode Kimetsu temporarily
hands stdin/stdout/stderr to the command, waits for it to exit, then redraws the
chat prompt.

Prefix modes avoid unnecessary model round trips:

```text
! cargo test -p kimetsu-chat
@crates/kimetsu-chat/src/repl.rs explain this function
$review inspect the current diff
```

`!` runs a captured shell command and adds the result to the transcript.
`@path` expands mentioned files into the next model request. `$skill` loads the
matching skill and applies it to the rest of the prompt.

`/plan` turns on read-only planning mode until you turn it off with
`/plan off`. `/compact` asks the selected model to summarize the session and
replace the transcript with that summary so long sessions stay within context.
`/review`, `/security-review`, and `/simplify` run focused read-only reviews of
the current git diff.

`/image <prompt>` is intentionally gated by the selected model. It only runs
when the model name is known to support image generation and `OPENAI_API_KEY`
is available. Non-image models keep the command disabled instead of pretending
the active model can draw.

## Hooks, MCP, Agents, And Tasks

Hooks are executable local scripts under `.kimetsu/hooks`, `.claude/hooks`, or
`.codex/hooks`. Supported event filenames are:

```text
session-start.ps1
session-end.ps1
pre-turn.ps1
post-turn.ps1
```

The same event names support `.cmd`, `.bat`, `.sh`, and `.exe`. Kimetsu runs
configured hooks with `KIMETSU_HOOK_EVENT`, `KIMETSU_WORKSPACE`,
`KIMETSU_SESSION_ID`, and `KIMETSU_INPUT` set in the environment. Hook failures
are surfaced in chat.

MCP supports stdio servers from `.kimetsu/mcp.json` or `.claude/mcp.json`:

```json
{
  "servers": {
    "example": {
      "command": "node",
      "args": ["server.js"],
      "env": { "EXAMPLE_MODE": "local" }
    }
  }
}
```

`/mcp tools <server>` initializes the server and calls `tools/list`.
`/mcp call <server> <tool> <json>` calls `tools/call`. This is a direct user
runtime today; model-visible MCP tools can be layered onto the dynamic tool
loader later.

Local agents are Markdown or JSON definitions under `.kimetsu/agents`,
`.claude/agents`, or `.codex/agents`. Markdown can use frontmatter:

```markdown
---
name: reviewer
description: Review diffs for correctness and test risk.
---
You are a strict code reviewer. Lead with findings.
```

Run one with `/agents run reviewer <prompt>`. The agent uses an isolated
system prompt and the current provider, then returns its answer into the chat.

`/tasks` manages in-session background commands. Output is written under
`.kimetsu/chat/tasks` and can be tailed with `/tasks output <id>`. Running
tasks are stopped when the chat session exits. Use `/tasks terminal <command>`
or `/tasks run --terminal <command>` for commands the user must navigate
directly; those run in the foreground and are not tracked as background tasks.

## Routing and Tool Loading

Kimetsu does not send every message through the full workspace-agent prompt.
Simple greetings and identity questions are answered locally. General
non-workspace questions use a text-only model route with no tool catalog.
Workspace questions use read-only tools, and code-changing tasks start with a
dynamic loadout that can call `load_tools` for edit, shell, background, image,
or full tool profiles only when the task needs them.

## Skills

Kimetsu can load Agent Skills, Codex skills, and Claude Code compatible skills.
A skill is the full folder bundle. `SKILL.md` is only the required entrypoint:
it provides `name` and `description` frontmatter plus activation
instructions, while bundled files stay available for on-demand use.

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

Kimetsu discovers the skill folder, injects the activated `SKILL.md`
instructions, and includes a compact resource inventory in the model context.
Bundled scripts, references, assets, templates, and other files are read or run
only when the active task calls for them. This follows the Agent Skills
progressive-disclosure model documented at <https://agentskills.io/home>.

Load skills at startup by name or path:

```pwsh
cargo run -p kimetsu-cli -- chat --workspace . --skill refactor
cargo run -p kimetsu-cli -- chat --workspace . --skill-dir C:\Users\you\.codex\skills --skill refactor
cargo run -p kimetsu-cli -- chat --workspace . --skill .\.claude\skills\frontend-design
```

Inside chat:

```text
/skills list
/skills sources
/skills search review
/skills select
/skills use refactor
/skills install refactor
/skills loaded
/skills clear
```

By default Kimetsu scans workspace `.codex/skills`, `.claude/skills`, and
`.kimetsu/skills`. The CLI also scans logged-in user tool homes:
`~/.codex/skills`, `~/.claude/skills`, `~/.agents/skills`, `~/.kimetsu/skills`,
and any plugin marketplace caches under `plugins/cache/*/*/*/skills`.

Use `--list-skill-sources` or `/skills sources` to see the detected provider
homes and marketplaces. Use `--list-skills` to print discovered skills, their
entrypoints, and bundled resource counts before exiting. Use `--search-skills`
or `/skills search <query>` to filter by name, description, origin, path, or
bundled resource.

In an interactive terminal, `/skills` and `/skills select` open a searchable
selector. Type to filter, use Up/Down to navigate, Space or Enter to load or
unload a skill for the current session, `i` to import an external provider
skill into `.kimetsu/skills`, `r` to refresh sources, and Esc to close.

Install/import a provider skill as a Kimetsu-owned bundle:

```pwsh
cargo run -p kimetsu-cli -- chat --workspace . --install-skill refactor
```

That copies the full skill folder into `.kimetsu/skills/<name>` and records
`.kimetsu-skill-origin.json` with the original provider, marketplace, and path.
Use `--install-skill-force` or `/skills install --force <name>` to replace an
existing import. Use `--no-workspace-skills` or `--no-user-skills` to narrow
discovery.

## Bridge And MCP Plugin

Kimetsu can act as the bridge between Claude Code, Codex, Agents, and its own
harness. Portable skills can be imported into `.kimetsu/extensions` and then
exported to another harness:

```pwsh
cargo run -p kimetsu-cli -- bridge scan --workspace .
cargo run -p kimetsu-cli -- bridge import reviewer --workspace .
cargo run -p kimetsu-cli -- bridge export reviewer codex --workspace .
cargo run -p kimetsu-cli -- bridge export reviewer claude --workspace .
cargo run -p kimetsu-cli -- bridge sync --workspace .
```

Install Kimetsu as the local MCP sidecar for host harnesses:

```pwsh
cargo run -p kimetsu-cli -- plugin install claude --mode optional --workspace .
cargo run -p kimetsu-cli -- plugin install codex --mode required --workspace .
cargo run -p kimetsu-cli -- mcp serve --workspace .
```

`--mode optional` makes Kimetsu brain the recommended first step and installs
soft-audit hooks. `--mode required` writes stronger Codex/Claude Code artifacts
plus hooks that treat missing Kimetsu brain context as a setup blocker for
non-trivial tasks unless the user explicitly waives Kimetsu.

The MCP server exposes:

```text
kimetsu_brain_status
kimetsu_brain_context
kimetsu_benchmark_context
kimetsu_benchmark_record_outcome
kimetsu_brain_memory_list
kimetsu_brain_memory_top
kimetsu_brain_memory_add
kimetsu_brain_memory_proposals
kimetsu_brain_memory_accept
kimetsu_brain_memory_reject
kimetsu_brain_memory_invalidate
kimetsu_brain_ingest_repo
kimetsu_bridge_status
kimetsu_skills_search
kimetsu_bridge_import
kimetsu_bridge_export
kimetsu_bridge_sync
kimetsu_plugin_install
```

Claude Code and Codex should call `kimetsu_benchmark_context` first on
Terminal-Bench tasks to retrieve a compact benchmark playbook and task-specific
outcome memories. Pass `warm_policy` as `cold_brain`, `reactive_warm`, or
`full_warm` when reproducing Claude Code brain-condition research. The
benchmark playbook ranks accepted `memory_role=semantic_operator` and
`memory_role=anti_pattern` memories ahead of exact episodic run summaries.
Those generalized memories are intended to transfer across task families; exact
`memory_role=episodic` outcomes are useful evidence but should not dominate the
plan.

For other broad tasks, call `kimetsu_brain_context` early to retrieve Kimetsu's
broker-ranked memory/repo/manifest capsules. After a benchmark attempt, call
`kimetsu_benchmark_record_outcome` so future runs can reuse the commands,
pitfalls, and verification steps. This always writes an accepted episodic
outcome memory. If the run revealed a reusable tactic or warning, also pass
`generalized_memory` with `memory_role=semantic_operator` or `anti_pattern`,
plus optional `task_family`, `applies_to`, `does_not_apply_to`,
`evidence_for`, `evidence_against`, and `generalization_rationale`. Kimetsu
stores that as a pending memory proposal so a human can accept only the lessons
that are broad enough to help future tasks. The bridge tools then handle
file-level portability work for skills and future extension types.

MCP descriptions and installed skills can make brain-first behavior explicit,
while generated hooks add runtime enforcement where the host runs local hooks.
Benchmark wrappers can additionally inspect `.kimetsu/hooks/usage/` markers or
MCP transcripts.
