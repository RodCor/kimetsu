<div align="center">

<img src="docs/assets/kimetsu-logo.png" alt="Kimetsu logo" width="220" />

# Kimetsu

### Give your coding agent a memory that gets sharper every run.

**Evidence-first memory for coding agents and Kimetsu's own terminal chat.**
Kimetsu sits beside your AI agent, watches what actually solves problems,
remembers it, and feeds the high-signal context back — so the next run
starts where the last one left off.

[![crates.io](https://img.shields.io/badge/crates.io-kimetsu--cli-orange)](https://crates.io/crates/kimetsu-cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![rust](https://img.shields.io/badge/rust-1.85%2B-informational)](https://www.rust-lang.org)

<img src="docs/assets/demo.gif" alt="Kimetsu demo: one-command setup, selftest, record a lesson, retrieve it by meaning" width="920" />

</div>

---

## At a glance

**Measured, not claimed** *(every number is reproducible with `kimetsu brain bench` / the ROI ledger)*:

| Metric | Result | How it's measured |
|--------|--------|-------------------|
| **Cost per win** | **$0.19 vs $2.47** (~13× cheaper) | 16-task Terminal-Bench slice, Kimetsu-on vs no-brain baseline |
| **Retrieval quality** | **recall@4 0.949 · MRR 0.914 @ ~138 ms** (default), up to **0.975 · 0.933** | `kimetsu brain bench`, 100-memory / 210-case dataset, jina-v2-base-code × cross-encoder rerank |
| **Scale** | ~1M memories in ~3 GB RAM, sub-2s retrieval | usearch HNSW ANN, O(log N) |
| **Footprint** | one SQLite file per project, **no cloud, no telemetry** | back it up with `cp` |

**Configurable surface** *(all in `.kimetsu/project.toml`, all optional with safe defaults)*:

| Section | Controls | Default |
|---------|----------|---------|
| `embedder` | semantic model + cross-encoder reranker (or FTS-only lean build) | jina-v2-base-code × ms-marco-tinybert |
| `[cheap_model]` | local **Ollama** or cloud model for digest / resume / `ask` / skill-draft — **optional**, degrades gracefully | off (rule-based) |
| `[storage] backend` | `flat` (FTS + ANN) · `graph-lite` (+ typed-edge hops) · `graph` (remote) | `flat` |
| `[broker]` | `warm_start`, `compress_capsules`, `session_dedupe`, `answer_grade_min_score`, `proactive_prefetch` | sensible on/off |
| `[sync]` | server-less multi-machine sync via a shared `dir` | off |
| `[learning]` | `store_queries` for the self-tuning eval set | on (local only) |

Full reference: **[How Kimetsu Works](docs/HOW-KIMETSU-WORKS.md)** · local models: **[docs/LOCAL-MODELS.md](docs/LOCAL-MODELS.md)**.

---

## Why Kimetsu

LLM coding agents are brilliant and forgetful. Every session starts from
zero — the same wrong turns, the same re-explaining of your conventions,
the same expensive exploration you already paid for last week.

Kimetsu fixes the forgetting. It's a **sidecar brain**: a single Rust binary
that runs next to any supported host agent through MCP (Claude Code, Codex, Pi,
OpenClaw, Cursor, Gemini CLI) or as its own terminal chat — or, in beta,
server-hosted over HTTP MCP and shared across a team. It learns which memories
the model *actually used to win*, and lets that knowledge compound across runs.

- **It remembers.** Project conventions, failure patterns, the exact command
  that regenerates your schema — captured once, retrieved automatically.
- **It learns what helps.** Memories that the model cites before solving a
  problem get promoted. Silent passengers and stale advice decay and get pruned.
- **It never explores twice** *(v2.0)*. A session-start digest plus an
  **episodic resume** mean the agent's first turn already knows the repo *and*
  what you were doing last time — no re-deriving the basics, no "where was I."
- **It answers, not just injects** *(v2.0)*. `kimetsu ask "…"` composes a
  grounded, cited answer from memory using a **local model** — zero frontier
  tokens, works offline. Lessons cited often graduate into runnable **skills**.
- **It's cheap to be right.** On a recorded 16-task Terminal-Bench slice,
  Kimetsu-enabled runs cost **~13x less per win** than the no-brain host-agent
  baseline: $0.19/win vs $2.47/win — and the `kimetsu brain roi` ledger shows
  the token savings per memory on your own work.
- **It gets smarter, not just bigger.** Semantic retrieval finds the right
  memory even when you used different words; it **self-tunes** retrieval against
  your own query history; and brain insights show hit-rate, citation rate, and
  token economy so the value is measurable.
- **It's yours, on your machine.** The whole brain is one SQLite file per
  project. No external vector DB, no cloud, no telemetry. Back it up with `cp`.

> *Kimetsu* (鬼滅) — "demon slayer." It slays the demon every agent fights:
> amnesia.

---

## How it works

```
  Host agent (Claude / Codex / Pi / OpenClaw / kimetsu chat)
       │  asks for context                    ▲ cites what helped
       ▼                                      │
  MCP tools ──► Broker ──► top memories ──► agent run
                  │  scores candidates by relevance ×
                  │  usefulness × freshness × scope
                  ▼
  brain.db — one SQLite file: FTS5 + semantic ANN (usearch HNSW)
```

1. **Before a task**, the broker walks your project brain *and* your
   cross-project user brain, scores every candidate, and injects the top few
   inside an adaptive token budget. The semantic build matches by *meaning*
   (O(log N) ANN — scales to ~1M memories in ~3 GB RAM, sub-2s retrieval).
2. **While it works**, Kimetsu surfaces known pitfalls *before* the first
   attempt, and the model cites the memories that actually help.
3. **After the task**, cited memories get promoted, unused advice decays on a
   half-life curve, and non-trivial sessions auto-harvest their lessons.

Full mechanics — scoring, citations, decay, conflict detection, the daemon —
in **[HOW-KIMETSU-WORKS](docs/HOW-KIMETSU-WORKS.md)**.

---

## Quickstart

```bash
# 1. Install — no Rust toolchain needed (cargo + prebuilt archives in docs/INSTALL.md)
npm install -g kimetsu-ai
kimetsu npm-flavor embeddings        # one-time: enable semantic retrieval

# 2. Wire it into your host agent — init + install + selftest in one shot
cd /your/project
kimetsu setup --host claude-code     # or: codex | openclaw | pi

# 3. Prove the brain works
kimetsu doctor --selftest
# ✓ recorded a memory and retrieved it — the brain works
```

From here your agent banks memories automatically — record one yourself and
watch it come back:

```bash
kimetsu brain memory add --scope project --kind convention "Use cargo nextest for all test runs"
kimetsu brain context "how do I run tests?"   # broker-ranked context bundle
kimetsu ask "how do I run tests?"             # v2.0: grounded, cited answer (local model)
kimetsu resume                                # v2.0: pick up where the last session left off
kimetsu brain insights                        # is the brain actually helping?
kimetsu brain roi --top                       # did it pay for itself? per-memory token savings
kimetsu brain tune --status                   # self-tune retrieval; --status shows when to re-run
kimetsu brain skills --review                 # v2.0: turn often-cited lessons into runnable skills
```

New in v2.0 — never explore twice:

```bash
kimetsu checkpoint "mid-task note"            # save working state now (auto-saved at session end)
kimetsu brain digest --refresh                # rebuild the session-start project digest
kimetsu brain sync                            # replicate your brain across machines (no server)
```

Prefer a standalone REPL? `kimetsu chat --workspace . --project .` is a full
terminal assistant with the same brain. Every install path (npm, prebuilt
archives), host-wiring details, the auto-harvest/distiller setup, and
maintenance commands live in **[docs/INSTALL.md](docs/INSTALL.md)**.

---

## Retrieval quality — benchmarked, not vibes

The semantic build retrieves with **jina-v2-base-code** + a
**cross-encoder reranker**, chosen with `kimetsu brain bench` on a
100-memory / 210-case dataset seeded from real exported memories. The
latency-optimized default (`ms-marco-tinybert-l-2-v2`) lands
**recall@4 0.949, MRR 0.914 at ~138 ms** per retrieval+rerank; the
quality-best rerankers reach **recall@4 0.975, MRR 0.933**. Swap embedder and
reranker with one config key each and re-judge on your own corpus — full grid
in [Retrieval models & benchmarking](docs/HOW-KIMETSU-WORKS.md).

---

## Kimetsu Remote (beta)

Share one brain per repository from a server, over HTTP MCP — for a team, or
for yourself across machines:

```bash
# server
kimetsu-remote serve --addr 0.0.0.0:8787 --data /srv/kimetsu-brains --token <secret>
# each client
kimetsu plugin install claude-code --remote https://kimetsu.example.com:8787
```

Bearer auth, per-repo brains, optional shared org-brain, server-side repo
ingest, TLS, Prometheus metrics, and a server-side reranker — full setup in
**[docs/REMOTE.md](docs/REMOTE.md)**.

---

## What's in the box

| Surface | What it is |
|---------|------------|
| **`kimetsu chat`** | A full terminal coding assistant — slash commands, skills, hooks, background tasks, MCP, agents. Runs against your workspace, no Harbor required. |
| **`kimetsu` brain** | Durable, auto-migrating project + user memory in a single SQLite file. Citations, decay, conflict detection, FTS + optional semantic (usearch HNSW ANN, scales to ~1M memories) retrieval, pluggable retrieval backends (`flat` / `graph-lite`), and `kimetsu brain insights` effectiveness analytics. |
| **`kimetsu ask` / warm-start** *(v2.0)* | Grounded, cited Q&A from memory via a local model (zero frontier tokens); a session-start digest + episodic **resume** so the first turn already knows the repo and your last task. |
| **`kimetsu brain skills`** *(v2.0)* | Often-cited lessons graduate into runnable, provenance-linked skills installed into your host's native skill dir — propose-only. |
| **`kimetsu brain sync`** *(v2.0)* | Server-less multi-machine sync via event-log replication over a shared folder (Dropbox / Syncthing / NAS). |
| **`kimetsu bridge`** | Cross-harness skill portability — import/export skills between supported hosts such as Claude Code, Codex, Agents, and Kimetsu. |
| **MCP sidecar** | `kimetsu mcp serve` exposes the brain to any MCP host as `kimetsu_*` tools, including `kimetsu_brain_answer` for mid-task grounded synthesis. |
| **Kimetsu Remote** *(beta)* | `kimetsu-remote` — the brain over HTTP MCP, one per repository, shared from a server (separate package). |

Built as a small Rust workspace (`kimetsu-cli`, `-chat`, `-agent`, `-brain`,
`-core`, and `-remote`). Lint + tests run clean on every change.

---

## Docs

- **[Install & host wiring](docs/INSTALL.md)** — every install path, plugin
  install/uninstall/status, auto-harvest + distiller, maintenance commands.
- **[How Kimetsu Works](docs/HOW-KIMETSU-WORKS.md)** — the conceptual reference:
  the brain, the broker, citations, decay, conflict detection, the MCP surface,
  retrieval models & benchmarking, the bridge, doctor, and config.
- **[Kimetsu Remote](docs/REMOTE.md)** — server setup, org brain, server-side
  ingest, TLS, client wiring.
- **[CHANGELOG](CHANGELOG.md)** — what shipped in each release.
- Per-crate `src/lib.rs` doc comments for module-level detail.

---

## License

Dual-licensed under [MIT](docs/LICENSE-MIT) or [Apache-2.0](docs/LICENSE-APACHE) — your
choice.
