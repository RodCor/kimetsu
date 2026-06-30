<div align="center">

<img src="docs/assets/kimetsu-logo.png" alt="Kimetsu logo" width="200" />

# Kimetsu

### Give your coding agent a memory that gets sharper every run.

*Kimetsu* (鬼滅), "demon slayer." It slays the demon every agent fights: amnesia.

[![crates.io](https://img.shields.io/badge/crates.io-kimetsu--cli-orange)](https://crates.io/crates/kimetsu-cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![rust](https://img.shields.io/badge/rust-1.85%2B-informational)](https://www.rust-lang.org)

<img src="docs/assets/demo.gif" alt="Kimetsu demo: one-command setup, selftest, record a lesson, retrieve it by meaning" width="720" />

</div>

---

## Why Kimetsu

LLM coding agents are brilliant and forgetful. Every session starts from zero:
the same wrong turns, the same re-explaining of your conventions, the same
expensive exploration you already paid for last week.

Kimetsu fixes the forgetting. It's a sidecar brain, a single Rust binary that
runs next to your host agent over MCP (Claude Code, Codex, Pi, OpenClaw, Cursor)
or as its own terminal chat. It learns which memories the model
actually used to win, and lets that knowledge compound across runs.

- **It remembers.** Project conventions, failure patterns, the exact command
  that regenerates your schema. Captured once, retrieved automatically.
- **It learns what helps.** Memories the model cites before solving a problem
  get promoted. Silent passengers and stale advice decay and get pruned.
- **It never explores twice.** A session-start digest and an episodic resume
  mean the agent's first turn already knows the repo and what you were doing
  last time. No re-deriving the basics, no "where was I."
- **It answers, not just injects.** `kimetsu ask` composes a grounded, cited
  answer from memory using a local model: zero frontier tokens, works offline.
  Lessons cited often enough graduate into runnable skills.
- **It's cheap to be right.** On a recorded 16-task Terminal-Bench slice, runs
  with Kimetsu cost about 13x less per win than the no-brain baseline ($0.19 vs
  $2.47), and the ROI ledger shows the token savings on your own work.
- **It gets smarter, not just bigger.** Semantic retrieval finds the right
  memory even when you used different words, and it self-tunes retrieval against
  your own query history.
- **It matches the paid clouds — for free.** On the public memory benchmarks
  (LongMemEval, BEAM) Kimetsu lands in the same accuracy band as mem0 / Zep, and
  at BEAM's matched 1M bucket it *edges* mem0's own number — with a retrieval
  pipeline that makes **zero LLM calls**: no API bill, no cloud, runs offline.
  ([benchmarks](#public-benchmarks--vs-other-memory-systems))
- **It's yours, on your machine.** The whole brain is one SQLite file per
  project. No external vector DB, no cloud, no telemetry. Back it up with `cp`.

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
  brain.db: one SQLite file, FTS5 + semantic ANN (usearch HNSW)
```

1. **Before a task**, the broker walks your project brain and your
   cross-project user brain, scores every candidate, and injects the top few
   inside an adaptive token budget. The semantic build matches by meaning
   (O(log N) ANN, scaling to ~1M memories in ~3 GB RAM, sub-2s retrieval).
2. **While it works**, Kimetsu surfaces known pitfalls before the first
   attempt, and the model cites the memories that actually help.
3. **After the task**, cited memories get promoted, unused advice decays on a
   half-life curve, and non-trivial sessions auto-harvest their lessons.

Full mechanics, scoring, citations, decay, conflict detection, and the daemon
are in **[How Kimetsu Works](https://rodcor.github.io/kimetsu/docs/how-kimetsu-works)**.

---

## Benchmarks

Every number is reproducible with `kimetsu brain bench` and the
`kimetsu brain roi` ledger.

| Metric | Result | How it's measured |
|--------|--------|-------------------|
| Cost per win | **$0.19 vs $2.47** (~13x cheaper) | 16-task Terminal-Bench slice, Kimetsu vs no-brain baseline |
| Retrieval quality | **recall@4 0.949, MRR 0.914 at ~138 ms** (default), up to 0.975 / 0.933 | `kimetsu brain bench`, 100-memory / 210-case dataset, jina-v2-base-code + cross-encoder rerank |
| Scale | ~1M memories in ~3 GB RAM, sub-2s retrieval | usearch HNSW ANN, O(log N) |
| Footprint | one SQLite file per project, no cloud, no telemetry | back it up with `cp` |

The semantic build retrieves with jina-v2-base-code and a cross-encoder
reranker, tuned with `kimetsu brain bench` on a 100-memory / 210-case dataset of
real exported memories. The latency-optimized default (ms-marco-tinybert-l-2-v2)
lands recall@4 0.949, MRR 0.914 at ~138 ms; the quality-best rerankers reach
recall@4 0.975, MRR 0.933. Swap embedder and reranker with one config key each
and re-judge on your own corpus. Full grid in
**[How Kimetsu Works](https://rodcor.github.io/kimetsu/docs/how-kimetsu-works)**.

Beyond recall, Kimetsu measures memory *correctness*: whether stale facts stay
out of retrieval and contradictions resolve to the current answer. Full
methodology and results in
**[the memory benchmark](https://rodcor.github.io/kimetsu/docs/memory-benchmark)**.

### Public benchmarks — vs other memory systems

Kimetsu's memory pipeline (ingest → store → retrieve → rerank) makes **zero LLM
calls**: FTS5 + local embeddings + a local cross-encoder — 100% local, free, and
offline-capable. mem0 / Zep / Letta call a model to *distill* memories at write
time **and** keep an LLM in the retrieval loop (mem0's own 2026 figures report
~7,000 tokens **per retrieval call** — a metered cost on every question). Kimetsu
lands in the **same accuracy band on the shared public benchmarks without the
LLM, the bill, or the cloud** — and at BEAM's matched 1M bucket it edges mem0
outright:

| benchmark | Kimetsu (local, model-free) | mem0 (vendor self-reported) |
|-----------|-----------------------------|------------------------------|
| **BEAM — 1M** (matched bucket) | **66.0%** | 62% |
| BEAM — 100K | 62.3% | — |
| LongMemEval (`_s`) | 79.5% (200-q) · ~77.2% weighted | 94.4% (full set, their reader) |

Honest, not cherry-picked: our LongMemEval is a 200-question slice (not the full
500), our BEAM-1M is 15 of 35 conversations with a Codex reader vs mem0's full set
on their own harness, and vendor numbers are self-reported (independent re-runs
routinely land lower — a published LoCoMo 91.6% reproduces nearer 58–66%). We ship
the exact harness, reader, and settings so ours can be checked. Per-ability
tables, caveats, and reproduction steps:
**[the memory benchmark](https://rodcor.github.io/kimetsu/docs/memory-benchmark)**.

---

## Quickstart

```bash
npm install -g kimetsu-ai
kimetsu npm-flavor embeddings        # one-time: enable semantic retrieval
cd /your/project
kimetsu setup --host claude-code     # or: codex | openclaw | pi
kimetsu doctor --selftest            # records a memory and retrieves it
```

Other install paths (cargo, prebuilt archives) and host-wiring details are in
**[the install guide](https://rodcor.github.io/kimetsu/docs/install)**.

---

## Command reference

| Command | What it does |
|---------|--------------|
| `kimetsu setup --host <h>` | Wire the brain into a host agent (init + install + selftest) |
| `kimetsu chat` | Standalone terminal coding assistant with the same brain |
| `kimetsu brain memory add` | Record a durable lesson by hand |
| `kimetsu brain context "<q>"` | Broker-ranked context bundle for a query |
| `kimetsu ask "<q>"` | Grounded, cited answer from memory (local model) |
| `kimetsu resume` / `kimetsu checkpoint` | Pick up where the last session left off |
| `kimetsu brain skills` | Turn often-cited lessons into runnable skills |
| `kimetsu brain insights` / `roi` | Is the brain helping, and did it pay for itself |
| `kimetsu brain tune` | Self-tune retrieval against your own query history |
| `kimetsu brain sync` | Replicate your brain across machines, no server |
| `kimetsu brain bench` | Benchmark retrieval on your own corpus |

The full command surface, configuration keys, and maintenance commands are in
**[How Kimetsu Works](https://rodcor.github.io/kimetsu/docs/how-kimetsu-works)** and
**[the install guide](https://rodcor.github.io/kimetsu/docs/install)**.

---

## Kimetsu Remote (beta)

Share one brain per repository from a server over HTTP MCP, for a team or for
yourself across machines:

```bash
# server
kimetsu-remote serve --addr 0.0.0.0:8787 --data /srv/kimetsu-brains --token <secret>
# each client
kimetsu plugin install claude-code --remote https://kimetsu.example.com:8787
```

Bearer auth, per-repo brains, an optional shared org-brain, server-side repo
ingest, TLS, Prometheus metrics, and a server-side reranker. Full setup in
**[the Kimetsu Remote guide](https://rodcor.github.io/kimetsu/docs/remote)**.

---

## What's in the box

| Component | What it is |
|-----------|------------|
| **The brain** | Durable project + user memory in one auto-migrating SQLite file: FTS + semantic retrieval, citations, decay, conflict detection, self-tuning, and effectiveness analytics. |
| **`kimetsu ask` + warm-start** | Grounded answers from memory, and a session-start digest plus episodic resume so the first turn already knows your work. |
| **`kimetsu chat`** | A full terminal coding assistant running against your workspace. |
| **MCP sidecar** | `kimetsu mcp serve` exposes the brain to any MCP host as `kimetsu_*` tools. |
| **Kimetsu Remote** *(beta)* | The brain over HTTP MCP, one per repository, shared from a server. |

Built as a small Rust workspace. Lint and tests run clean on every change.

---

## Docs

- **[Install & host wiring](https://rodcor.github.io/kimetsu/docs/install)**: every install path, host
  wiring, auto-harvest and distiller setup, maintenance commands.
- **[How Kimetsu Works](https://rodcor.github.io/kimetsu/docs/how-kimetsu-works)**: the brain, the broker,
  citations, decay, conflict detection, the MCP surface, retrieval models and
  benchmarking, configuration, the bridge, and doctor.
- **[Local models](https://rodcor.github.io/kimetsu/docs/local-models)**: run fully local with Ollama.
- **[Kimetsu Remote](https://rodcor.github.io/kimetsu/docs/remote)**: server setup, org brain, TLS, clients.
- **[CHANGELOG](https://rodcor.github.io/kimetsu/docs/changelog)**: what shipped in each release.

---

## License

Dual-licensed under [MIT](docs/LICENSE-MIT) or [Apache-2.0](docs/LICENSE-APACHE),
your choice.
