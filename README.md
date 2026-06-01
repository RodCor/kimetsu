<div align="center">

<img src="docs/assets/kimetsu-logo.png" alt="Kimetsu logo" width="220" />

# Kimetsu

### Give your coding agent a memory that gets sharper every run.

**Evidence-first memory for MCP-capable coding agents and Kimetsu's own terminal chat.**
Kimetsu sits beside your AI agent, watches what actually solves problems,
remembers it, and feeds the high-signal context back — so the next run
starts where the last one left off.

[![crates.io](https://img.shields.io/badge/crates.io-kimetsu--cli-orange)](https://crates.io/crates/kimetsu-cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![rust](https://img.shields.io/badge/rust-1.85%2B-informational)](https://www.rust-lang.org)

</div>

---

## Why Kimetsu

LLM coding agents are brilliant and forgetful. Every session starts from
zero — the same wrong turns, the same re-explaining of your conventions,
the same expensive exploration you already paid for last week.

Kimetsu fixes the forgetting. It's a **sidecar brain**: a single Rust binary
that runs next to any supported host agent through MCP (including Claude Code
and Codex) or as its own terminal chat, learns which memories the model
*actually used to win*, and lets that knowledge compound across runs.

- **It remembers.** Project conventions, failure patterns, the exact command
  that regenerates your schema — captured once, retrieved automatically.
- **It learns what helps.** Memories that the model cites before solving a
  problem get promoted. Silent passengers and stale advice decay and get pruned.
- **It's cheap to be right.** On a recorded 16-task Terminal-Bench slice,
  Kimetsu-enabled runs cost **~13x less per win** than the no-brain host-agent
  baseline: $0.19/win vs $2.47/win.
- **It's yours, on your machine.** The whole brain is one SQLite file per
  project. No vector DB, no cloud, no telemetry. Back it up with `cp`.

> *Kimetsu* (鬼滅) — "demon slayer." It slays the demon every agent fights:
> amnesia.

---

## How it works

```
   +----------------------------+
   | Host agent                 |
   | Claude Code / Codex / chat |
   +-------------+--------------+
                 |
                 | asks for context
                 v
   +-------------+--------------+        +------------------------------+
   | MCP tool surface           |        | Kimetsu brain                |
   | kimetsu_brain_context      | -----> | brain.db                     |
   | cite_memory / record       |        | SQLite + FTS5 + embeddings   |
   +-------------+--------------+        +---------------+--------------+
                 |                                       ^
                 | candidates                            |
                 v                                       |
   +-------------+--------------+                        |
   | Broker                     |                        |
   | scores + ranks by:         |                        |
   | relevance, usefulness,     |                        |
   | freshness, scope           |                        |
   +-------------+--------------+                        |
                 |                                       |
                 | top context                           |
                 v                                       |
   +-------------+--------------+                        |
   | Agent run                  |                        |
   | uses context, cites memory | -----------------------+
   | outcomes update ranking    | citations + outcomes
   +----------------------------+
```

1. **Before a task**, the agent asks Kimetsu for context. The **broker**
   walks your project brain *and* your cross-project user brain, scores every
   candidate memory (relevance × usefulness × freshness × scope), de-duplicates,
   and injects the top few inside a token budget.
2. **During the task**, the model calls `cite_memory` when a memory actually
   helps. Those citations are the ground truth.
3. **After the task**, Kimetsu rewards cited memories, lightly nudges the
   "silent passengers," and lets old advice decay on a half-life curve. The
   brain gets sharper with every run — automatically.

Want the full mechanics — scoring weights, citation deltas, decay, conflict
detection? See **[docs/HOW-KIMETSU-WORKS.md](docs/HOW-KIMETSU-WORKS.md)**.

---

## Install

Kimetsu is a single Rust binary. Pick your flavor:

```bash
# Default lean build — fast lexical (FTS) retrieval, no model download
cargo install kimetsu-cli

# Semantic build — fastembed + ONNX; first run downloads BGE-small
cargo install kimetsu-cli --features embeddings

# From source
cargo install --path crates/kimetsu-cli   # add --features embeddings for semantic search
```

Prefer not to touch the Rust toolchain? Pre-built binaries for
**Linux / macOS / Windows** ship on every
[GitHub Release](https://github.com/RodCor/kimetsu/releases). Extract the archive and put
`kimetsu` / `kimetsu.exe` somewhere on `PATH` (`~/.local/bin`, `/usr/local/bin`,
or `%USERPROFILE%\.cargo\bin`). Lean archives are published for Linux,
macOS Intel, macOS Apple Silicon, and Windows. Embeddings archives are
published where ONNX Runtime prebuilts are available: Linux x86_64,
macOS Apple Silicon, and Windows x86_64.

Confirm it's healthy:

```bash
kimetsu --version
kimetsu doctor      # checks paths, brain.db, embedder, MCP, bridge
```

Check for updates:

```bash
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

**Prerequisites:** Rust 1.85+ (stable) and a model credential for the surface
you use (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`, or `OPENAI_API_KEY`).
That's it for chat — Docker, Harbor, and Python are only needed for benchmark runs.

---

## Quick start

### 1. Talk to it directly

```bash
kimetsu chat --workspace . --project .
```

`--project .` turns on memory: Kimetsu keeps one brain session open for the
whole conversation and injects retrieved context into every turn. Inside chat,
`/help` lists everything; favorites: `/plan`, `/run`, `/verify`, `/review`,
`/skills`, `/cost`, and `$skill <prompt>` to apply a skill.

### 2. Or bolt it onto a host agent

Wire Kimetsu into any supported host as an MCP sidecar. The built-in installers
cover Claude Code and Codex:

```bash
kimetsu plugin install claude --workspace .    # writes .mcp.json + .claude/settings.json
kimetsu plugin install codex  --workspace .    # writes .codex/config.toml + .codex/hooks.json + skill
```

Now your host agent gets the `kimetsu_*` MCP tools (brain context, memory
add/list, citations, repo ingest, the cross-harness skill bridge) and starts
banking memories across every session.

```bash
kimetsu brain search "build failures"
kimetsu brain context "where is auth configured?"
kimetsu brain memory top          # most useful memories so far
```

---

## What's in the box

| Surface | What it is |
|---------|------------|
| **`kimetsu chat`** | A full terminal coding assistant — slash commands, skills, hooks, background tasks, MCP, agents. Runs against your workspace, no Harbor required. |
| **`kimetsu` brain** | Event-sourced project + user memory in SQLite. Citations, decay, conflict detection, FTS + optional semantic retrieval. |
| **`kimetsu bridge`** | Cross-harness skill portability — import/export skills between supported hosts such as Claude Code, Codex, Agents, and Kimetsu. |
| **MCP sidecar** | `kimetsu mcp serve` exposes the brain to any MCP host as `kimetsu_*` tools. |

Built as a small Rust workspace (`kimetsu-cli`, `-chat`, `-agent`, `-brain`,
and `-core`). Lint + tests run clean on every change.

---

## Docs

- **[How Kimetsu Works](docs/HOW-KIMETSU-WORKS.md)** — the conceptual reference:
  the brain, the broker, citations, decay, conflict detection, the MCP surface,
  the bridge, doctor, and config. Start here for depth.
- **[CHANGELOG](CHANGELOG.md)** — what shipped in each release.
- Per-crate `src/lib.rs` doc comments for module-level detail.

---

## License

Dual-licensed under [MIT](docs/LICENSE-MIT) or [Apache-2.0](docs/LICENSE-APACHE) — your
choice.
