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

## What is Kimetsu

Coding agents are brilliant and forgetful. Every session starts from zero: the
same wrong turns, the same re-explained conventions, the same exploration you
already paid for last week.

Kimetsu is a sidecar brain for your agent. One Rust binary and one SQLite file
per project, wired into Claude Code, Codex, Pi, OpenClaw, or Cursor over MCP,
or driven from its own terminal chat. It captures the lessons an agent earns,
learns which ones actually help, and hands them back before the next task. The
memory pipeline calls no LLM: storage and retrieval are 100% local, free, and
offline-capable.

| | |
|---:|---|
| **89.4%** | LoCoMo, the long-conversation memory benchmark, full 1,540-question set |
| **73.3%** | BEAM 100K memory benchmark, matching the prior public state of the art, model-free |
| **66.0%** | BEAM 1M, ahead of mem0's self-reported 62% at the same bucket |
| **83.0%** | LongMemEval, the public long-term-memory benchmark |
| **13x** | cheaper per solved task: $0.19 vs $2.47 on a 16-task Terminal-Bench slice |
| **~1M** | memories held in ~3 GB RAM with sub-2s retrieval, one SQLite file |
| **$0** | API cost to store and recall: zero LLM calls in the memory pipeline |

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
**[the install guide](https://kimetsu.dev/docs/install)**.

---

## What it does

- **Remembers what matters.** Project conventions, failure patterns, the exact
  command that regenerates your schema. Captured once, retrieved by meaning,
  even when you phrase it differently.
- **Speaks first.** Most memory waits to be asked. Kimetsu is proactive: a
  session-start digest, an episodic resume, and pre-task context mean the
  agent's first turn already knows the repo, your conventions, and what you
  were doing last time.
- **Learns what helps.** Memories the model cites before solving a problem get
  promoted. Stale advice and silent passengers decay and get pruned.
- **Answers, not just injects.** `kimetsu ask` composes a grounded, cited
  answer from memory using a local model: zero frontier tokens, works offline.
  Lessons cited often enough graduate into runnable skills.
- **Pays for itself.** ~13x cheaper per solved task than a no-brain baseline on
  a recorded Terminal-Bench slice, and the ROI ledger shows the savings on your
  own work.
- **Stays yours.** The whole brain is one SQLite file per project. No external
  vector DB, no cloud, no telemetry. Back it up with `cp`.

---

## How it works

<img src="docs/assets/how-it-works.svg" alt="How Kimetsu works: the host agent asks the broker for context, the broker scores candidates from brain.db by relevance, usefulness, freshness, and scope, injects the top memories into the agent run, and the run cites what helped so cited memories rise and stale ones decay" width="960" />

1. **Before a task**, the broker walks your project brain and your
   cross-project user brain, scores every candidate, and injects the top few
   inside an adaptive token budget.
2. **While it works**, Kimetsu surfaces known pitfalls before the first
   attempt, and the model cites the memories that actually help.
3. **After the task**, cited memories get promoted, unused advice decays on a
   half-life curve, and non-trivial sessions auto-harvest their lessons.

Full mechanics: scoring, citations, decay, conflict detection, retrieval
levels, and the daemon, in
**[How Kimetsu Works](https://kimetsu.dev/docs/how-kimetsu-works)**.

---

## Share your brain

A brain is a portable file. Export it as a pack, hand it to a teammate, merge
theirs into yours, or swap a whole brain in and out. Onboard a new machine or a
new hire with one import.

```bash
# export a shareable pack (always gzip-compressed, always security-scrubbed)
kimetsu brain export onboarding.json.gz --name rust-conventions --version 1.0.0

# merge a pack into your brain (additive, dedups against what you already know)
kimetsu brain import onboarding.json.gz

# install straight from a URL
kimetsu brain import https://example.com/packs/rust-conventions.json.gz

# swap: replace your current memories in the pack's scope (reversible)
kimetsu brain import other-brain.json.gz --mode replace --yes
```

Every export is scrubbed before it leaves your machine: credentials and PII
are redacted automatically, and `--strict` aborts the export if anything was
found. Merge is idempotent, so re-importing is safe. Replace supersedes rather
than deletes, so a swap can always be undone. For continuous sharing,
`kimetsu brain sync` replicates a brain across machines with no server, and
**[Kimetsu Remote](https://kimetsu.dev/docs/remote)** serves one
brain per repository to a whole team.

---

## Benchmarks vs other memory systems

Kimetsu's memory pipeline (ingest, store, retrieve, rerank) makes **zero LLM
calls**: FTS5 + local embeddings + a local cross-encoder. mem0 / Cognee / Zep /
Letta call a model to distill memories at write time **and** keep an LLM in the
retrieval loop (mem0's own 2026 figures report ~7,000 tokens per retrieval
call, a metered cost on every question). Kimetsu lands in the same accuracy
band without the LLM, the bill, or the cloud.

| benchmark | Kimetsu (local, model-free) | mem0 | Cognee |
|-----------|-----------------------------|------|--------|
| **BEAM 1M** (matched bucket) | **66.0%** | 62% | not reported |
| BEAM 100K | **73.3%** | n/a | 79% |
| BEAM 10M | future work | 48.6% | 67% |
| LongMemEval (`_s`) | **83.0%** (200-q) · ~80.9% weighted | 94.4% (full set, their reader) | not reported |

Honest, not cherry-picked: our LongMemEval is a 200-question slice (not the
full 500), our BEAM-1M is 15 of 35 conversations with a Codex reader vs mem0's
full set on their own harness, Cognee (a knowledge-graph system with an LLM in
the loop) leads at 100K/10M, and vendor numbers are self-reported. We ship the
exact harness, reader, and settings so ours can be checked. Per-ability tables,
caveats, and reproduction steps:
**[the memory benchmark](https://kimetsu.dev/docs/memory-benchmark)**.

Retrieval itself is benchmarked too: recall@4 0.949 and MRR 0.914 at ~138 ms
with the default reranker (up to 0.975 / 0.933 with the quality-best one), on a
210-case dataset of real exported memories. Reproduce or re-tune on your own
corpus with `kimetsu brain bench`.

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
| `kimetsu brain export` / `import` | Share brains: scrubbed packs, merge or replace, file or URL |
| `kimetsu brain sync` | Replicate your brain across machines, no server |
| `kimetsu brain skills` | Turn often-cited lessons into runnable skills |
| `kimetsu brain insights` / `roi` | Is the brain helping, and did it pay for itself |
| `kimetsu brain tune` | Self-tune retrieval against your own query history |
| `kimetsu brain bench` | Benchmark retrieval on your own corpus |

The full command surface, configuration keys, and maintenance commands are in
**[How Kimetsu Works](https://kimetsu.dev/docs/how-kimetsu-works)** and
**[the install guide](https://kimetsu.dev/docs/install)**.

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
**[the Kimetsu Remote guide](https://kimetsu.dev/docs/remote)**.

---

## Docs

- **[Install & host wiring](https://kimetsu.dev/docs/install)**: every install path, host
  wiring, auto-harvest and distiller setup, maintenance commands.
- **[How Kimetsu Works](https://kimetsu.dev/docs/how-kimetsu-works)**: the brain, the broker,
  citations, decay, conflict detection, the MCP surface, retrieval models and
  benchmarking, configuration, the bridge, and doctor.
- **[Local models](https://kimetsu.dev/docs/local-models)**: run fully local with Ollama.
- **[Kimetsu Remote](https://kimetsu.dev/docs/remote)**: server setup, org brain, TLS, clients.
- **[CHANGELOG](https://kimetsu.dev/docs/changelog)**: what shipped in each release.

---

## License

Dual-licensed under [MIT](docs/LICENSE-MIT) or [Apache-2.0](docs/LICENSE-APACHE),
your choice.
