# RFC: Kimetsu v3.0 — the memory that speaks first

Status: **draft** · Target: v3.0 · Supersedes: nothing · Author: Kimetsu maintainers

---

## Why

Kimetsu's thesis is that memory should be **proactive**: it comes to you, you
don't fetch it. That thesis is right, and — as of mid-2026 — nobody else in the
coding-agent space is executing on it. Every competing memory layer (ByteRover,
Supermemory, agentmemory, Mem0, Zep, Cognee) is a pull-based MCP server the
agent has to remember to call.

Two things are true at once:

1. **The lane is open.** The research has caught up to the thesis and validated
   it. *Remember When It Matters* (arXiv 2607.08716) runs a separate memory
   agent beside an unmodified action agent, deciding turn by turn whether to
   inject a reminder or stay silent — worth +8.3 pp on Terminal-Bench 2.0 and
   +6.8 pp on τ²-Bench. That is Kimetsu's design, measured.
2. **We are not yet executing on it as fully as the README claims.** An audit
   of all four repos found the proactive surface real on Claude Code, thin on
   Codex, and — until this RFC's first patches — entirely absent on Cursor, Pi
   and OpenClaw.

This RFC is the honest version of where the package stands and what v3.0 does
about it.

---

## Part 1 — What the audit found

### A. The proactivity claim held on one host

| Claim | Reality before v3.0 |
|---|---|
| "Speaks first" | True on **Claude Code**. Cursor has no shell-hook surface, so it got nothing. Codex has no `SessionStart` event, so the digest and episodic resume never reached it. |
| Pi / OpenClaw | **Injected nothing.** Both spawned `kimetsu` with `stdio: "ignore"`: the hook's `additionalContext` went to `/dev/null`, and the hook read an empty stdin and bailed on its minimum-prompt guard. Kimetsu was write-only on those hosts. |
| MCP surface | **Pull-only by construction.** No sampling, no elicitation, no resources, no server→client notifications; the read loop drops inbound notifications, and nothing writes to stdout except in reply to an `id`-bearing request. All proactivity rides on host hooks. |
| "Surfaces known pitfalls" | Real-time monitoring is a case-insensitive substring scan for ten words (`error`, `failed`, `fatal`, `panic`, …), `PostToolUse`, Bash only. No exit code, no stderr/stdout split, no structured parsing. |
| "Warns before the first attempt" | `PreToolUse` is Bash-only; `broker.proactive_prefetch` — which lets it warn about a *file* you are about to edit — defaults to `false`. |
| Background reflection | **None.** The "daemon" is an embedding/rerank cache with a 300 ms client budget and no scheduler. Consolidation, conflict resolution, pruning, tuning, reindex and skill graduation are all manual CLI invocations. |
| Skills graduation | Detection exists, but its only caller is the `brain skills` CLI. The loop never closes on its own. |

Plus four concrete defects, all fixed in the first v3.0 patch set (see
[Part 3, phase 0](#phase-0--defects-landed)):

- **Source drift** between the Pi extension this repo installs and the one the
  `kimetsu-pi` npm package publishes. The npm copy had timeout hardening the
  installed copy never got.
- **Doc drift**: `interfaces.md` advertised `kimetsu_skill`, `cite_memory` and
  `expand_capsule` as MCP tools. The first does not exist; the other two are
  tools of the autonomous agent pipeline, not the MCP surface.
- **A dead path**: `digest::is_stale()` was implemented and documented as
  driving a detached rebuild, with no production caller.
- **An undocumented halving**: capsules fill against `budget_tokens / 2`, so
  every published budget number was double the real one.

### B. The benchmarked configuration was not the default

The published **BEAM 100K 73.3%** was measured on the **graph-lite** backend.
`[storage] backend` defaults to `flat`, which scored **62.3%** on the same set:
an 11-point gap between the advertised number and the out-of-the-box one. And
graph-lite degenerates to flat unless `kimetsu brain graph build` has been run,
because the only edges normally present are `supersedes`, which retrieval
already excludes. The reserved edge types `refines`, `dead_end_of`,
`decision_touches` and `lesson_from` are declared and essentially never written.

Separately, the `embeddings` cargo feature is off by default, so a stock
`cargo install kimetsu-cli` gets `NoopEmbedder`: no cosine, no ANN, no semantic
dedup, no conflict detection, no reranker. The npm flavor enables it; the
crates.io path does not.

### C. The weak abilities are architectural, not tuning

| Ability | Score | Root cause |
|---|---|---|
| event ordering (BEAM 100K / 1M) | **32.5% / 30%** | no ordering structure exposed to retrieval; `work_episodes` never participates in scoring |
| abstention | **45% / 30%** | Kimetsu abstains at the *bundle* level, but never signals insufficiency inside a bundle it does return |
| knowledge update (100K) | 60% | contradiction detection is cosine proximity, not entailment |
| temporal reasoning @1M | 50% | uni-temporal only; no as-of queries |
| multi-session (LongMemEval) | **58.8%** | no entity layer; multi-hop is a co-citation staple, not a graph walk |
| preference following (LongMemEval) | **66.7%** | user modelling is one memory kind plus a second SQLite file — no profile, no schema, no preference-aware ranking prior |
| BrainBench dedup / importance / calibration | 77% / 76% / 82% | calibration is the thinnest track |
| BEAM 10M | not run | needs the write-time distiller in the loop |

Two engine-level notes behind those numbers:

- **Fusion is union-max plus a linear α = 0.5 blend**, with per-kind max
  normalization — so the top memory and the top repo_file each get
  `relevance = 1.0` regardless of absolute quality, and the lexical/semantic
  floors exist to compensate. RRF is the 2026 default for precisely this
  score-incompatibility problem.
- **Tags are not a column.** They live inline as `[tags: …]` text, are
  re-parsed on every use, and the tag boost is a substring match on the
  lowercased summary.

### D. Package-level gaps

- Benchmarks are partial: LongMemEval is a 200/500 slice, BEAM-1M is 15/35
  conversations, BEAM 10M unrun. `kimetsu-bench` ships five drivers and no
  coding-agent memory benchmark, while the field now has SWE-ContextBench,
  SWE-MeM, RoadmapBench and ChainSWE. The 13× cost claim rests on a 16-task
  Terminal-Bench slice.
- **No TypeScript SDK.** `kimetsu-py` is a complete typed client;
  `npm/kimetsu` and `npm/kimetsu-remote` are binary-download shims. The
  ecosystem Kimetsu targets — Pi extensions, OpenClaw plugins, Cursor, VS Code,
  MCP clients — is overwhelmingly TypeScript, and every integration currently
  shells out to the binary and parses text. The asset-drift defect above is a
  direct symptom.
- **No memory-safety story.** Nothing addresses memory poisoning (OWASP ASI06;
  MINJA reports >95% injection success against memory-backed agents),
  provenance-weighted trust, or memory-induced sycophancy — where MemSyco-Bench
  finds most memory systems score *worse* than using no memory at all.

---

## Part 2 — The 2026 landscape

**What the competition ships that we don't.** Zep/Graphiti: a bitemporal
knowledge graph where a contradicting fact *invalidates* rather than
overwrites, so historical questions still answer. Hindsight (BEAM-10M SOTA,
64.1%): TEMPR — four strategies per query (semantic, BM25, graph traversal,
temporal) fused by RRF then cross-encoder reranked — plus entity resolution and
auto-updating mental models. Mem0: native in-process graph memory with entity
dedup and three-tier user/session/agent scopes. Cognee: 14 retrieval modes,
leading BEAM 100K/10M with an LLM in the loop. Letta: **sleep-time compute** — a
second agent reorganizes memory during downtime and pre-computes answers to
anticipated queries (2.5× improvement; same accuracy at roughly half the
test-time tokens). EverMemOS: MemCells consolidated into thematic MemScenes
with time-bounded foresight signals.

**What we should steal, with sources.**

| Idea | Source | Where it lands |
|---|---|---|
| Inject-or-stay-silent as a learned decision | *Remember When It Matters*, 2607.08716 | phase 3b |
| Idle-time compute against predicted queries | *Sleep-time Compute*, 2504.13171; *Anticipate and Learn*, 2605.25971 | phase 3a |
| User profile as an explicit ranking prior | *PPRO / Learning User-Aware Recall*, 2607.00017 | phase 2h |
| Contexts as evolving playbooks, incremental delta updates | *ACE*, 2510.04618 | phases 2c, 3d |
| Bitemporal validity with invalidation, not overwrite | Zep/Graphiti | phase 2f |
| RRF over per-strategy result lists | Hindsight TEMPR; hybrid-search practice | phase 2a |
| Sycophancy and poisoning as measurable properties | MemSyco-Bench 2607.01071; MINJA 2601.05504 | phase 4 |

**Where we should refuse to follow.** *Anatomy of Agentic Memory* (2602.19320)
finds that benchmarks are underscaled and saturating, metrics are misaligned
with semantic utility, and **system-level latency and throughput cost is
routinely ignored**. A model-free, local, single-SQLite-file memory is the
system best placed to win on that axis. v3.0 does not trade it away.

---

## Part 3 — The plan

### The two-tier product

v3.0 stops pretending there is one configuration. There are two, named and
benchmarked separately:

- **Kimetsu Free** — the default. Zero LLM calls in the memory pipeline. The
  current claim, unchanged and still the headline.
- **Kimetsu Deep** — opt-in `[kimetsu] tier = "deep"`. A local small model
  (Ollama, or the existing distiller credential path) in the loop for the write
  distiller, digest distillation, reflection, entailment-based contradiction
  detection, the inject-or-stay-silent policy, and idle-time anticipation.

**Invariant: every Deep feature has a Free fallback that is already today's
behaviour.** Both tiers get a column in every table in `docs/memory-benchmark/`.
Nothing about Deep is allowed to erode the Free claim.

### Phase 0 — Defects (landed)

1. **Pi and OpenClaw actually inject.** The generated extensions now spawn with
   piped stdio, write the hook payload to stdin, read stdout, and return the
   parsed `additionalContext` through the host's own contract — Pi's
   `before_agent_start` message, OpenClaw's `agent_turn_prepare`
   `prependContext`. Every failure mode stays a silent no-op.
2. **One source of truth per integration asset.** The templates moved from
   string literals in `bridge.rs` to `crates/kimetsu-chat/assets/`, pulled in
   with `include_str!`. `scripts/sync-pi-package.sh` vendors the Pi extension
   into the npm package, and a CI job diffs the two on every PR.
3. **Warm start on every host.** The per-turn hook takes
   `--warm-on-first-prompt` and prepends the digest + resume to the session's
   first turn — covering Codex, Pi, OpenClaw, and anything else with only a
   per-turn hook. Claude Code does not pass it and keeps its `SessionStart`
   route, so nobody gets the block twice.
4. **Cursor.** No hooks at all, so the first `kimetsu_brain_context` call of a
   session returns a `warm_start` field alongside the capsules, and the Cursor
   rule file tells the agent to make that call at task start. The latch is
   process-scoped on the stdio path only; the remote server, which fans one
   process across many sessions, deliberately skips it.
5. **`digest::is_stale()` is wired.** A cached digest is served immediately even
   when the corpus has moved under it, and the rebuild is spawned detached —
   instead of putting a synchronous rebuild in front of the first turn.
6. **Doc and comment truth-up.** The phantom MCP tools are gone from
   `interfaces.md` (with `cite_memory` / `expand_capsule` correctly relocated to
   the agent-pipeline docs), the stale `graph-lite` / `graph` TODOs are
   replaced with what those backends actually do, and the `budget_tokens / 2`
   halving is documented in `the-broker.md`.

Still open in this phase, gated on measurement rather than opinion:

7. **Ship the benchmarked configuration by default** — default
   `[storage] backend` to `graph-lite` and write `relates_to` edges on the
   ingest path (phase 2c) so the graph is non-empty without a manual
   `brain graph build`. Gate the flip on `kimetsu brain bench` plus a
   BrainBench run showing no regression. Enable `embeddings` by default for the
   `kimetsu-cli` binary target so `cargo install` matches the npm flavor, while
   library crates stay lean.

### Phase 1 — Tiers

Add `[kimetsu] tier`, route the Deep-only paths through it, and split the
benchmark tables. Mechanical, but it has to land before phase 2g and 3b, which
are the first features with a genuine Deep variant.

### Phase 2 — Retrieval and accuracy

- **2a. RRF fusion** across the FTS, ANN, and (new) graph and temporal candidate
  lists, replacing union-max plus the linear α blend. Keep the blend behind a
  config key and sweep `fusion ∈ {rrf, linear}` in `brain tune`.
- **2b. Global normalization** in place of per-kind max normalization — the
  thing the lexical and semantic floors exist to compensate for.
- **2c. First-class entities and edges.** `memory_tags` and `memory_entities`
  tables; parse `[tags: …]` once at write time rather than on every read; write
  `relates_to` edges on ingest; start emitting the declared-but-unused
  `refines` / `dead_end_of` / `lesson_from` types from the supersede, episode
  and distiller paths.
- **2d. Event ordering** (32.5% → target 70%+): an `occurred_at` ordering
  signal, `work_episodes` exposed to retrieval as an ordered episodic strand,
  and ordered capsules rendered with explicit timestamps.
- **2e. Abstention** (45% → target 75%+): a calibrated evidence-coverage score
  on `ContextBundle`, rendered as an explicit "memory does not cover X" line, so
  a reader can abstain on Kimetsu's advice instead of confabulating from partial
  evidence.
- **2f. Bitemporal**: `recorded_at` / `recorded_to` beside `valid_from` /
  `valid_to`, plus an `--as-of` query mode.
- **2g. Entailment-based contradiction (Deep)**: a local NLI cross-encoder
  adjudication step, with cosine proximity as the Free path, and a resolution
  score that is confidence-dominant rather than recency-dominant.
- **2h. User profile as a ranking prior**: a `user_profile` projection from
  preference memories plus citation history, contributing a `profile_affinity`
  term to the broker score.
- **2i. BEAM 10M** with the write-time distiller in the loop.

### Phase 3 — Proactive autonomy

- **3a. A real daemon.** Promote the embed daemon into a scheduled background
  worker with an idle detector. Free tier runs the existing model-free passes on
  a schedule: `reinforce`, conflict resolution, pruning, digest refresh,
  trigger-gated self-tuning, skill-graduation detection. Deep adds reflection
  and sleep-time anticipation. Everything interruptible; nothing may hold the
  300 ms retrieval budget.
- **3b. A learned inject-or-stay-silent policy**, replacing fixed thresholds,
  trained locally on the regret sidecar and citation outcomes over features
  already being logged: score, kind, novelty, repeat count, prior injection
  acceptance, time since last injection. Free tier fits a logistic model with no
  LLM; Deep adds model adjudication. **This is the highest-leverage item in the
  RFC.**
- **3c. Real error monitoring**: exit-code inspection, stderr/stdout separation,
  and per-toolchain parsers (cargo, npm/vitest, pytest, go test) in place of the
  ten-word substring list.
- **3d. Close the skills loop**: graduation detection from the daemon, with
  newly graduated skills surfaced in the warm start.
- **3e. Graduate `proactive_prefetch` to default-on**, gated on a measured
  false-positive rate from the regret data.

### Phase 4 — Safety and trust

No competitor owns this ground.

- **4a. Provenance-weighted trust**: a per-memory trust term from origin, source
  kind (manual > distilled > imported pack > remote org brain), and citation
  outcomes, entering the broker score and gating import.
- **4b. Poisoning resistance**: quarantine imported and remote-origin memories
  until corroborated by a local citation; anomaly-detect write bursts; add
  `kimetsu brain audit` for post-hoc causal attribution. The event-sourced
  design makes this unusually cheap — the audit trail already exists.
- **4c. Sycophancy resistance**: render memory as a claim to verify rather than
  as ground truth, enforce scope validity at render time, and add a MemSyco-style
  track to BrainBench.
- **4d. Belief-drift detection**: an embedding-only per-session drift signal
  (Nautilus Compass reaches ROC AUC 0.83 on real Claude Code traces using
  nothing but cosine against behavioural anchors) — a natural Free-tier feature
  given the warm embedder already in the daemon.

### Phase 5 — Benchmarks and SDK

- BEAM 10M; LongMemEval to the full 500; BEAM-1M to 35/35; a MemSyco-derived
  sycophancy track; a poisoning track; and a coding-agent memory benchmark in
  the SWE-ContextBench mould.
- A TypeScript SDK (`npm/kimetsu-sdk`) mirroring `kimetsu-py`, so integrations
  call a typed client instead of shelling out and parsing text.

---

## What this RFC does not claim

- Phase 0 is landed and tested; **everything from phase 1 onward is a proposal**,
  not a commitment, and each item gates on a measurement before merge.
- The default-configuration flip (phase 0 item 7) is not done. Until it is,
  reproducing the published BEAM 100K figure requires
  `[storage] backend = "graph-lite"` and a `kimetsu brain graph build`.
- Target numbers in phase 2 are targets, not results.
- Competitor figures cited here are self-reported unless otherwise noted, and
  the 2026 roundups show several of them not reproducing. Ours ship with the
  harness so they can be checked.
