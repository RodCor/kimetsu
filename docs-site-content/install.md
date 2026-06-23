---
title: 'Install'
sidebar_position: 3
---

# Installing Kimetsu

Kimetsu is a single Rust binary. There's really only one choice to make at
install time — **lean vs semantic (embeddings)** — because that's the only part
baked into the binary. *Which host agents you use* (Claude Code, Codex, Pi,
OpenClaw) is a **runtime** choice you change anytime with `kimetsu plugin
install`/`uninstall` — no reinstall. The official prebuilt + npm binaries
include all six host integrations; a bare source `cargo install` is minimal and
adds them with `--features pi,openclaw`.

## cargo

```bash
# Default lean build — fast lexical (FTS) retrieval, no model download
cargo install kimetsu-cli

# Semantic build — fastembed + ONNX; first run downloads the embedding model
cargo install kimetsu-cli --features embeddings

# Add the Pi + OpenClaw host integrations to a source build (prebuilts already have them)
cargo install kimetsu-cli --features pi,openclaw
# Everything:
cargo install kimetsu-cli --features embeddings,pi,openclaw

# From source
cargo install --path crates/kimetsu-cli   # add --features embeddings,pi,openclaw for full build
```

## npm

Installs the prebuilt binary for your platform, no Rust required:

```bash
npm install -g kimetsu-ai          # lean build (all host integrations included)
kimetsu npm-flavor embeddings      # one-time: switch to the semantic build — it persists
```

npm pulls only the matching per-platform package (`@kimetsu-ai/*`) via
optionalDependencies — there's no postinstall download, so it works under
`npm install --ignore-scripts`. **`kimetsu npm-flavor embeddings`** fetches the
semantic build once and remembers the choice (no env var to keep exported);
`kimetsu npm-flavor lean` switches back, and `kimetsu npm-flavor status` shows
the current one. (The `KIMETSU_NPM_FLAVOR` env var still works as a per-run
override.) The embeddings build is available where ONNX Runtime prebuilts exist
(Linux x64, macOS Apple Silicon, Windows x64); elsewhere it stays lean. See
[`npm/`](https://github.com/RodCor/kimetsu/tree/main/npm) for details.

## Pre-built archives

For **Linux / macOS / Windows** on every
[GitHub Release](https://github.com/RodCor/kimetsu/releases). Extract the archive and put
`kimetsu` / `kimetsu.exe` somewhere on `PATH` (`~/.local/bin`, `/usr/local/bin`,
or `%USERPROFILE%\.cargo\bin`). Every prebuilt archive — lean and embeddings —
bundles all six host integrations, so switching hosts never needs a reinstall.
Lean archives are published for Linux,
macOS Intel, macOS Apple Silicon, and Windows. Embeddings archives are
published where ONNX Runtime prebuilts are available: Linux x86_64,
macOS Apple Silicon, and Windows x86_64.

## Health check, updates, uninstall

```bash
kimetsu --version
kimetsu doctor      # checks paths, brain.db, embedder, MCP, bridge

kimetsu update --check
kimetsu update          # updates discovered kimetsu binaries on PATH/current install
kimetsu uninstall --dry-run
kimetsu uninstall --yes # removes discovered kimetsu binaries
```

`kimetsu update` downloads the matching GitHub Release archive for your
platform and flavor, then updates the current executable plus verified
`kimetsu` copies in known install locations such as Cargo bin, `~/.local/bin`,
`/usr/local/bin`, or `%USERPROFILE%\.cargo\bin`. It does not scan the whole
disk. `kimetsu uninstall` removes those same verified binaries; it leaves
project `.kimetsu/` directories and the user brain intact unless you explicitly
pass `--delete-user-data`.

**Prerequisites:** Rust 1.85+ (stable, source builds only) and a model
credential for the surface you use (`CLAUDE_CODE_OAUTH_TOKEN`,
`ANTHROPIC_API_KEY`, or `OPENAI_API_KEY`).
On AWS Bedrock, set `[model] provider = "bedrock"` and authenticate with
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (+ optional `AWS_SESSION_TOKEN`)
and `AWS_REGION` — the agent and the auto-harvester both support it, and can be
pointed at different providers. That's it for chat — Docker, Harbor, and Python
are only needed for benchmark runs.

## Wiring host agents

Wire Kimetsu into any supported host. The built-in installers cover Claude Code,
Codex, Pi, OpenClaw, Cursor, and Gemini CLI:

```bash
kimetsu plugin install claude      --workspace .  # writes .mcp.json + .claude/settings.json
kimetsu plugin install codex       --workspace .  # writes .codex/config.toml + .codex/hooks.json + skill + agent
kimetsu plugin install openclaw    --workspace .  # MCP server + hooks plugin + skill in .openclaw/ (requires --features openclaw on source builds)
kimetsu plugin install pi          --workspace .  # TS extension (Pi has no MCP) + skill in .pi/ (requires --features pi on source builds)
kimetsu plugin install cursor      --workspace .  # writes .cursor/mcp.json + .cursor/rules/kimetsu-brain/rule.md
kimetsu plugin install gemini-cli  --workspace .  # writes .gemini/settings.json + merges GEMINI.md into project root

# Install globally for every project (writes to the host's home config dir):
kimetsu plugin install claude --scope global

# See what's wired where, or remove just the wiring (keeps the binary + brain):
kimetsu plugin status
kimetsu plugin uninstall codex --yes

# Or do init + install + selftest in one shot:
kimetsu setup --host claude-code

# Switched editors? Move your wiring — no reinstall (prebuilt/npm binaries
# include every host; on a source build add `--features pi`):
kimetsu plugin uninstall claude-code --yes   # drop the old host's wiring
kimetsu plugin install pi                     # wire the new one
```

**Cursor and Gemini CLI:** neither host has a `UserPromptSubmit`-style hook
system, so MCP + an always-on guidance file are the complete integration
surface for those hosts (no automatic prompt-time context injection; the
model must call `kimetsu_brain_context` manually). The config schemas for
Cursor (`.cursor/mcp.json`) and Gemini CLI (`.gemini/settings.json`) match each
host's current official MCP documentation (re-verified June 2026): Cursor uses
`mcpServers` with `type: "stdio"`, `command`, and `args`; Gemini CLI uses
`mcpServers` with `command` and `args` (transport inferred — no `type` field).
The installer merges non-destructively, so any existing servers are preserved.

`--scope` defaults to `workspace`. The installer **merges** into existing
config: if you already have hooks — even on the same events Kimetsu uses
(`UserPromptSubmit`, `PreToolUse`, …) — your hooks are kept and Kimetsu's are
added alongside them. Re-running is idempotent and never needs `--force`.

Now your host agent gets the `kimetsu_*` MCP tools (brain context, memory
add/list, citations, repo ingest, the cross-harness skill bridge) and starts
banking memories across every session.

## Auto-harvest and the SessionEnd distiller

Memories get **auto-harvested**: when you fix a command that was failing,
or finish a non-trivial session without recording anything, a Kimetsu hook cues
the agent to dispatch a background `kimetsu-memory-harvester` subagent (a cheap
in-agent distiller) that records the lesson for next time — no extra API key.
Turn it off with `[learning] auto_harvest = false` in `.kimetsu/project.toml`.

For a deterministic harvest that doesn't depend on the agent, `kimetsu plugin
install claude-code` and `kimetsu plugin install codex` offer to set up a
**SessionEnd distiller**: a cheap configured model (Anthropic
`claude-haiku-4-5`, OpenAI `gpt-5.4-mini`, or a compatible endpoint via
`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`) that distills each session itself at
the end and records the lessons. Claude Code runs it from `SessionEnd`; Codex
runs it from the supported `Stop` hook with `--distill-on-stop`. The wizard
stores the key in a gitignored `.env`; skip it with `--no-setup`. Run it with
`--scope global` to configure the distiller once in
`~/.kimetsu/` — it then distills every project's sessions into your user brain
(available everywhere), unless that project has its own distiller.

## Configuration toggles

Every optional feature is turn-off-able in `.kimetsu/project.toml` —
embeddings (`[embedder] enabled`), ambient workspace context
(`[broker] ambient`), the global user brain (`[kimetsu] use_user_brain`),
auto-harvest, the distiller, secret redaction. The precedence is
**env override > config > default**, and `kimetsu config edit` opens the file
in `$EDITOR` and re-validates on save. Re-installing merges, so your toggles
survive.

## Maintenance & lifecycle

```bash
kimetsu config set embedder.enabled false   # flip any toggle (config get reads one)
kimetsu brain export mem.json                # move memories between brains (import reads them)
kimetsu brain memory edit <id> --text "…"    # fix a recording in place (undo retires the last one)
kimetsu runs prune --older-than 30d          # drop old run dirs; brain compact VACUUMs brain.db
kimetsu ps                                   # see running MCP servers; stop clears a stale one
kimetsu uninstall                            # tiered: binary / + plugin wiring / + brains
```

`.kimetsu/` stays lean — just `brain.db` + `project.toml`; transient
proactive/chat/bench output lives under `~/.kimetsu/cache/`.

For retrieval model selection (embedder/reranker swapping, benchmarking your
own corpus), see
[Retrieval models & benchmarking in How Kimetsu Works](how-kimetsu-works).
