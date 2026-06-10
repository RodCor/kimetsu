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
  memory even when you used different words, and brain insights show you the
  hit-rate, citation rate, and token economy so the value is measurable.
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
# 1. Install (cargo shown; npm + prebuilt archives in docs/INSTALL.md)
cargo install kimetsu-cli --features embeddings   # or plain for the lean FTS build

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
kimetsu brain insights                        # is the brain actually helping?
```

Prefer a standalone REPL? `kimetsu chat --workspace . --project .` is a full
terminal assistant with the same brain. Every install path (npm, prebuilt
archives), host-wiring details, the auto-harvest/distiller setup, and
maintenance commands live in **[docs/INSTALL.md](docs/INSTALL.md)**.

---

## Retrieval quality — benchmarked, not vibes

The semantic build retrieves with **jina-v2-base-code** + a
**cross-encoder reranker** (`ms-marco-tinybert-l-2-v2`), chosen with
`kimetsu brain bench` on a 100-memory / 210-case dataset seeded from real
exported memories: **recall@4 0.949, MRR 0.914 at ~132ms** per
retrieval+rerank (FTS-only baseline: MRR ~0.81). Swap models with one config
key and re-judge on your own corpus — see
[Retrieval models & benchmarking](docs/HOW-KIMETSU-WORKS.md).

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
| **`kimetsu` brain** | Durable, auto-migrating project + user memory in a single SQLite file. Citations, decay, conflict detection, FTS + optional semantic (usearch HNSW ANN, scales to ~1M memories) retrieval, and `kimetsu brain insights` effectiveness analytics. |
| **`kimetsu bridge`** | Cross-harness skill portability — import/export skills between supported hosts such as Claude Code, Codex, Agents, and Kimetsu. |
| **MCP sidecar** | `kimetsu mcp serve` exposes the brain to any MCP host as `kimetsu_*` tools. |
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
