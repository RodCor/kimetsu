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
that runs next to any supported host agent through MCP (Claude Code, Codex, Pi,
OpenClaw) or as its own terminal chat — or, in beta, server-hosted over HTTP MCP
and shared across a team. It learns which memories the model *actually used to
win*, and lets that knowledge compound across runs.

- **It remembers.** Project conventions, failure patterns, the exact command
  that regenerates your schema — captured once, retrieved automatically.
- **It learns what helps.** Memories that the model cites before solving a
  problem get promoted. Silent passengers and stale advice decay and get pruned.
- **It's cheap to be right.** On a recorded 16-task Terminal-Bench slice,
  Kimetsu-enabled runs cost **~13x less per win** than the no-brain host-agent
  baseline: $0.19/win vs $2.47/win.
- **It gets smarter, not just bigger.** Semantic retrieval finds the right
  memory even when you used different words; the agent surfaces known pitfalls
  *before* it repeats them; and brain insights show you the hit-rate,
  citation rate, and token economy so the value is measurable, not a vibe.
- **It's yours, on your machine.** The whole brain is one SQLite file per
  project — `.kimetsu/` is just `brain.db` plus a `project.toml`. No external
  vector DB, no cloud, no telemetry. It auto-migrates forward on upgrade
  (backing itself up first). Back it up with `cp`.

> *Kimetsu* (鬼滅) — "demon slayer." It slays the demon every agent fights:
> amnesia.

---

## How it works

```
   +----------------------------+
   | Host agent                 |
   | Claude / Codex / Pi /      |
   | OpenClaw / chat            |
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
   and injects the top few inside an adaptive token budget. On the semantic
   build it also runs an approximate-nearest-neighbour index (usearch HNSW) so a
   memory surfaces even when the query shares no words with it — O(log N) per
   query, scaling to ~1M memories in ~3 GB RAM with sub-2s retrieval.
2. **While it works**, Kimetsu is proactive: it surfaces "known pitfalls"
   before the first attempt, classifies the task to bias which kinds of memory
   it recalls, and the model calls `cite_memory` when a memory actually helps.
   Those citations are the ground truth.
3. **After the task**, Kimetsu rewards cited memories, lightly nudges the
   "silent passengers," and lets old advice decay on a half-life curve. The
   brain gets sharper with every run — automatically.

The whole brain is one auto-migrating SQLite file: `brain.db`'s `events` table
is the durable log, so `.kimetsu/` stays lean (just `brain.db` + `project.toml`)
and upgrades migrate forward with a backup taken first.

Want the full mechanics — scoring weights, semantic retrieval, the proactive
agent brain, citation deltas, decay, conflict detection? See
**[docs/HOW-KIMETSU-WORKS.md](docs/HOW-KIMETSU-WORKS.md)**.

---

## Install

Kimetsu is a single Rust binary. There's really only one choice to make at
install time — **lean vs semantic (embeddings)** — because that's the only part
baked into the binary. *Which host agents you use* (Claude Code, Codex, Pi,
OpenClaw) is a **runtime** choice you change anytime with `kimetsu plugin
install`/`uninstall` — no reinstall. The official prebuilt + npm binaries
include all four host integrations; a bare source `cargo install` is minimal and
adds them with `--features pi,openclaw`.

```bash
# Default lean build — fast lexical (FTS) retrieval, no model download
cargo install kimetsu-cli

# Semantic build — fastembed + ONNX; first run downloads BGE-small
cargo install kimetsu-cli --features embeddings

# Add the Pi + OpenClaw host integrations to a source build (prebuilts already have them)
cargo install kimetsu-cli --features pi,openclaw
# Everything:
cargo install kimetsu-cli --features embeddings,pi,openclaw

# From source
cargo install --path crates/kimetsu-cli   # add --features embeddings,pi,openclaw for full build

### Retrieval quality (benchmarked defaults)

The embeddings build retrieves with **jina-v2-base-code** (embedder) +
**ms-marco-tinybert-l-2-v2** (cross-encoder reranker), chosen with
`kimetsu brain bench` on a 100-case dataset built from real exported
memories: **recall@4 0.966, MRR 0.938, ~43ms per rerank, ~4× less
off-topic noise** than the bge-small baseline (FTS-only scores MRR ~0.81).
Swap models with `kimetsu config set embedder.model|reranker …` (then
`kimetsu brain reindex`), and re-judge on your own corpus with
`kimetsu brain bench` — see "Retrieval models & benchmarking" in
[HOW-KIMETSU-WORKS](docs/HOW-KIMETSU-WORKS.md).
```

Prefer not to touch the Rust toolchain? Two options.

**npm** — installs the prebuilt binary for your platform, no Rust required:

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
[`npm/`](npm/) for details.

**Pre-built archives** — for **Linux / macOS / Windows** on every
[GitHub Release](https://github.com/RodCor/kimetsu/releases). Extract the archive and put
`kimetsu` / `kimetsu.exe` somewhere on `PATH` (`~/.local/bin`, `/usr/local/bin`,
or `%USERPROFILE%\.cargo\bin`). Every prebuilt archive — lean and embeddings —
bundles all four host integrations, so switching hosts never needs a reinstall.
Lean archives are published for Linux,
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
On AWS Bedrock, set `[model] provider = "bedrock"` and authenticate with
`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (+ optional `AWS_SESSION_TOKEN`)
and `AWS_REGION` — the agent and the auto-harvester both support it, and can be
pointed at different providers. That's it for chat — Docker, Harbor, and Python
are only needed for benchmark runs.

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

Wire Kimetsu into any supported host. The built-in installers cover Claude Code,
Codex, Pi, and OpenClaw:

```bash
kimetsu plugin install claude   --workspace .  # writes .mcp.json + .claude/settings.json
kimetsu plugin install codex    --workspace .  # writes .codex/config.toml + .codex/hooks.json + skill + agent
kimetsu plugin install openclaw --workspace .  # MCP server + hooks plugin + skill in .openclaw/ (requires --features openclaw on source builds)
kimetsu plugin install pi       --workspace .  # TS extension (Pi has no MCP) + skill in .pi/ (requires --features pi on source builds)

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

`--scope` defaults to `workspace`. The installer **merges** into existing
config: if you already have hooks — even on the same events Kimetsu uses
(`UserPromptSubmit`, `PreToolUse`, …) — your hooks are kept and Kimetsu's are
added alongside them. Re-running is idempotent and never needs `--force`.

Now your host agent gets the `kimetsu_*` MCP tools (brain context, memory
add/list, citations, repo ingest, the cross-harness skill bridge) and starts
banking memories across every session.

Memories also get **auto-harvested**: when you fix a command that was failing,
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

### 3. Or share one brain from a server (Kimetsu Remote — **beta**)

> **Beta.** Kimetsu Remote is under active testing and may have rough edges or
> breaking changes before the stable release. The `kimetsu-remote` **server is a
> separate package** — `cargo install kimetsu-cli` / `npm i -g kimetsu-ai` do
> **not** install it. Install it on the server when you want it:
>
> ```bash
> npm install -g kimetsu-remote                       # prebuilt server binary
> cargo install kimetsu-remote --features embeddings  # or from source
> ```
>
> (or grab the standalone `kimetsu-remote` archive from a GitHub Release). The
> `kimetsu plugin install --remote` *client* wiring is part of the normal
> `kimetsu` binary — no separate install needed to point a host at a server.

Run the brain on a server and connect over **HTTP MCP**, so a team — or you
across machines — shares one brain per repository, with no local checkout:

```bash
# On the server (build with --features embeddings for semantic retrieval):
kimetsu-remote serve --addr 0.0.0.0:8787 --data /srv/kimetsu-brains \
  --token <secret> --rate-limit 120        # 120 req/min per token (0 = off)
#   one brain per repo under <data>/<repo-id>/; bearer-auth; plain HTTP — put a
#   TLS proxy (nginx/Caddy) in front, or build `--features tls` and pass
#   --tls-cert/--tls-key for in-process HTTPS. `GET /healthz` and `GET /metrics`
#   (Prometheus text, aggregate-only) are unauthenticated. Prebuilt
#   kimetsu-remote binaries are built with embeddings + TLS support.
#
#   Add --org-brain /srv/kimetsu-org for a shared team brain: memories recorded
#   at `global_user` scope land there and merge into EVERY repo's retrieval
#   (project-scoped memories stay per-repo). Must be outside --data.
#
#   Add --repos-file repos.toml --checkout-dir /srv/checkouts to let the server
#   clone registered repos and ingest their files (remote file-capsule retrieval).

# On each client — wire a host at the remote instead of the local stdio command:
kimetsu plugin install claude-code --remote https://kimetsu.example.com:8787
kimetsu plugin install openclaw    --remote https://kimetsu.example.com:8787
```

The repo id is derived from your git remote (`--repo <id>` to override), so the
endpoint becomes `https://…/mcp/<repo-id>`. By default the host config
references `${KIMETSU_REMOTE_TOKEN}` (set that env var where your agent runs)
rather than writing the token to disk; pass `--token <t>` to embed a literal.
The remote surfaces the memory/retrieval/curation tools by default.

**Retrieval quality.** The server reranks `kimetsu_brain_context` results with a
cross-encoder (`--reranker`, default `jina-reranker-v1-tiny-en`, operator-level —
`"off"` disables, any curated/HF id accepted). Benchmark results on the 100-memory
dataset (production floors active, jina-tiny reranker):

| embedder          | MRR   | seq mean | rps  | peak RSS |
|-------------------|-------|----------|------|----------|
| jina-v2-base-code | 0.906 | 416ms    |  5.0 | 1.2 GB   |
| bge-small-en-v1.5 | 0.909 | 700ms    |  3.8 |  697 MB  |

The embedder is set per-repo via config or `KIMETSU_BRAIN_EMBEDDER`; the reranker
is operator-owned and cannot be overridden by a repo's `project.toml`.
See §7a "Retrieval models on the server" in
[HOW-KIMETSU-WORKS.md](docs/HOW-KIMETSU-WORKS.md) for the full table and
how to re-run the benchmark.

**Server-side ingest (optional).** To make file-capsule retrieval work remotely,
let the server keep a managed clone of each repo. The operator pre-registers
repos in a TOML file (so clients can't make the server clone arbitrary URLs):

```toml
# repos.toml
[repos]
github-com-org-api = { url = "https://github.com/org/api.git", branch = "main" }
github-com-org-web = "https://github.com/org/web.git"
```

```bash
kimetsu-remote serve --data /srv/kimetsu-brains --token <secret> \
  --repos-file /etc/kimetsu/repos.toml --checkout-dir /srv/kimetsu-checkouts
```

Then `kimetsu_brain_ingest_repo` clones/refreshes the registered repo and indexes
its files into that repo's brain, so `context` retrieval includes file capsules.
Private repos use the server's own git auth (credential helper / SSH / a token in
the URL). The repo-id keys must match the ids clients connect with.

```bash
kimetsu brain search "build failures"
kimetsu brain context "where is auth configured?"
kimetsu brain memory top          # most useful memories so far
kimetsu brain insights            # is the brain actually helping?
```

Every optional feature is turn-off-able in `.kimetsu/project.toml` —
embeddings (`[embedder] enabled`), ambient workspace context
(`[broker] ambient`), the global user brain (`[kimetsu] use_user_brain`),
auto-harvest, the distiller, secret redaction. The precedence is
**env override > config > default**, and `kimetsu config edit` opens the file
in `$EDITOR` and re-validates on save. Re-installing merges, so your toggles
survive.

### Maintenance & lifecycle

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

---

## 5-minute quickstart — prove it works

**Step 1: Install**

```bash
cargo install kimetsu-cli           # lean build (FTS retrieval)
# or with semantic search:
cargo install kimetsu-cli --features embeddings
```

**Step 2: Wire it into your host agent**

```bash
cd /your/project
kimetsu init                                 # creates .kimetsu/project.toml + brain.db
kimetsu plugin install claude --workspace .  # Claude Code: writes .mcp.json + hooks
# or: codex | openclaw | pi
kimetsu plugin install codex --workspace .   # Codex: writes .codex/ config + hooks
```

(Or collapse all three steps into one: `kimetsu setup --host claude-code`.)

**Step 3: Verify the brain is working**

```bash
kimetsu doctor --selftest
# prints: ✓ recorded a memory and retrieved it — the brain works
```

**Step 4: Record your first memory**

From the command line:

```bash
kimetsu brain memory add --scope project --kind convention "Use cargo nextest for all test runs"
```

Or let the agent record it — inside Claude Code or Codex, the agent calls
`kimetsu_brain_record` after any non-trivial solve. The Stop hook prints a
summary at the end of each session.

**Step 5: Retrieve it**

```bash
kimetsu brain search "test runs"          # lexical FTS search
kimetsu brain context "how do I run tests?"  # broker-ranked context bundle
kimetsu brain memory top                  # most-useful memories by score
kimetsu brain insights                    # effectiveness analytics
```

From this point your agent automatically retrieves the top context capsules
before each task. Cite a memory to give it a +1 usefulness signal;
memories the agent never reaches for decay slowly and can be pruned
with `kimetsu brain memory prune`.

**Troubleshoot:** `kimetsu doctor` checks paths, brain.db schema, embedder,
MCP wiring, and installed hooks. `kimetsu doctor --selftest` is the one-shot
"confirm it works end-to-end" check.

---

## What's in the box

| Surface | What it is |
|---------|------------|
| **`kimetsu chat`** | A full terminal coding assistant — slash commands, skills, hooks, background tasks, MCP, agents. Runs against your workspace, no Harbor required. |
| **`kimetsu` brain** | Durable, auto-migrating project + user memory in a single SQLite file. Citations, decay, conflict detection, FTS + optional semantic (usearch HNSW ANN, scales to ~1M memories) retrieval, and `kimetsu brain insights` effectiveness analytics. |
| **`kimetsu bridge`** | Cross-harness skill portability — import/export skills between supported hosts such as Claude Code, Codex, Agents, and Kimetsu. |
| **MCP sidecar** | `kimetsu mcp serve` exposes the brain to any MCP host as `kimetsu_*` tools. |
| **Kimetsu Remote** *(beta)* | `kimetsu-remote` — the brain over HTTP MCP, one per repository, shared from a server (separate package). |

Built as a small Rust workspace (`kimetsu-cli`, `-chat`, `-agent`, `-brain`,
`-core`, and `-remote`). Lint + tests run clean on every change.

---

## Docs

- **[How Kimetsu Works](docs/HOW-KIMETSU-WORKS.md)** — the conceptual reference:
  the brain, the broker, citations, decay, conflict detection, the MCP surface,
  Kimetsu Remote, the bridge, doctor, and config. Start here for depth.
- **[CHANGELOG](CHANGELOG.md)** — what shipped in each release.
- Per-crate `src/lib.rs` doc comments for module-level detail.

---

## License

Dual-licensed under [MIT](docs/LICENSE-MIT) or [Apache-2.0](docs/LICENSE-APACHE) — your
choice.
