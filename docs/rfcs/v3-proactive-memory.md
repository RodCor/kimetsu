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

### Phase 1 — Tiers (landed)

`[kimetsu] tier` names the two pipelines. Every automatic model call in the
memory pipeline resolves through `distiller::resolve_pipeline_distiller`, which
returns `None` on Free, so each caller falls back to the rule-based path it
already had — the guarantee is a property of the code, not of configuration.

Leave the field unset for **auto**: a brain with a cheap model configured is
already making model calls, so it reads as Deep, which keeps every pre-v3.0
config behaving exactly as before. `tier = "free"` is a durable opt-out with
credentials present; `tier = "deep"` with no reachable model resolves back down
and says so in `doctor` and `brain status`.

Gated (automatic, in-pipeline): session-end distillation, episode capture, HyDE
query expansion, the stop hook's distiller branch. Ungated (invoked by name):
`ask`, `brain reflect`, `brain distill`, `brain skills`, `doctor` — silently
refusing an explicit request would be a worse lie than the one the gate
prevents.

### Phase 2 — Retrieval and accuracy

- **2a. RRF fusion (landed).** `[broker] fusion` selects `linear` or `rrf`, with
  a per-request override so `brain tune` can compare them in one process against
  one corpus; the sweep grid gained the dimension (80 combos → 160), and
  `--apply` writes the winner. The default stays `linear`: RRF being the 2026
  default for hybrid retrieval is a fact about the literature, not a measurement
  on this corpus, and the house rule is that every claim ships with one. The
  sweep is how a corpus gets to say otherwise.
- **2b. Global normalization** in place of per-kind max normalization — the
  thing the lexical and semantic floors exist to compensate for. **Still open**,
  and for the same reason: it is a ranking change that needs a measurement on a
  semantic build to justify, not an argument.
- **2c. First-class entities and edges (landed).** Schema v11 adds
  `memory_entities`, a projection of each memory's tags and salient terms, with
  the author-supplied tag distinguished from a term the extractor guessed. The
  write path links each memory as it lands via one indexed lookup, asking for
  two shared entities rather than the batch builder's one — on the write path a
  single shared word would attach every new memory to half the corpus. Emitting
  the declared-but-unused `refines` / `dead_end_of` / `lesson_from` types is
  still open.
- **2d. Event ordering** (32.5% → target 70%+): **landed**, though not as
  planned. The plan proposed an `occurred_at` signal and an episodic strand in
  scoring. Reading the render path showed the problem is not in retrieval at
  all: memories carry `created_at`, capsules do not, and the bundle is rendered
  as an unordered relevance-sorted set with no dates. The information is found
  and selected and then discarded at the last step, so a reader asked which came
  first has nothing to order by and guesses — a coin flip at two events, which
  is what 30% looks like. So the fix is at render time: on a query carrying an
  ordering marker, re-render the bundle oldest-first with each memory's date in
  front of its text, under a line saying so. It runs after the budget loop, so
  it cannot change *which* capsules ship — only how they read. Adding a scoring
  signal was never necessary, and would have been the more invasive change.
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

- **3a. Background upkeep (landed, not as a daemon).** The embed daemon stays a
  dumb model cache — anything it does slowly is something the hook waits for.
  Instead each pass records when it last ran, `due_passes()` answers what is
  overdue, and the session-start and session-end hooks fire a detached
  `kimetsu brain maintain` and return immediately: no resident process, no timer
  thread, nothing to supervise. Reinforce, digest, prune-candidate detection and
  skill-candidate detection, each on its own interval. A failing pass is not
  marked as run, so it retries next tick rather than being skipped for its
  interval. All model-free; reflection and sleep-time anticipation are still
  open, and both are Deep-tier by construction.
- **3b. A learned inject-or-stay-silent policy (landed).** A logistic
  regression over score, loop mode, capsule kind, session novelty, repeat count,
  recovery since the last injection, and the strength of the failure evidence.
  Its prior is solved by hand so the decision boundary is *exactly* the old
  thresholds with every other weight at zero — an untrained brain behaves
  precisely as it did, so there is no cold-start regression to trade against the
  gain. Labels come from `proactive.injected` events joined to citations: an
  injection the agent never cited is the definition of an interruption that was
  not worth making. Refuses to fit below 40 examples or with one class present.
  `kimetsu brain policy` prints the weights, `--train` refits, `--reset` returns
  to the constant. Deep-tier model adjudication is still open.
- **3c. Real error monitoring (landed).** `tool_outcome::classify` answers "did
  that fail?" from the exit code when the harness reports one, then a toolchain
  summary line (cargo, libtest, pytest, jest/vitest, go test, tsc, npm, make),
  then the substring scan — which an explicit `0 failed` / `test result: ok` now
  vetoes. A passing test suite is no longer a failure because its summary line
  contains the word "failed". The verdict carries its evidence tier, and the
  toolchain parsers extract the real diagnostic, which is a much better
  retrieval query and loop signature than "first line containing error".
- **3d. Close the skills loop**: graduation detection from the daemon, with
  newly graduated skills surfaced in the warm start.
- **3e. Graduate `proactive_prefetch` to default-on**, gated on a measured
  false-positive rate from the regret data.

### Phase 4 — Safety and trust

No competitor owns this ground.

- **4a. Provenance-weighted trust (landed).** Retrieval discounts a memory by
  origin: distilled 0.95, remote 0.90, pack 0.85, local and derived unchanged.
  Corroboration — a citation in a successful run here, which is exactly what
  `last_useful_at` records — erases the discount outright, so an imported pack
  is not second-class forever. Unknown provenance reads as local, so upgrading
  does not make an existing brain untrusted. Trust is a weight, never a gate: a
  bad import must not be able to silently delete working knowledge.
- **4b. Poisoning resistance (partly landed).** `kimetsu brain audit` groups the
  corpus by origin, reports the uncorroborated external population, and flags
  write bursts — clusters that arrived faster than anyone types, the shape both
  a bulk import and induced poisoning leave. Read-only: an automated purge on
  that heuristic would delete a legitimate import. **Quarantining imports is
  now landed**, in the import path rather than behind a scoring weight. It
  routes an imported pack into the existing review queue, on by default for
  `http(s)://` sources and off for a local file. One part of the plan was not
  implementable as written: "quarantine until corroborated by a local citation"
  cannot fire, because a memory outside the retrieval pool can never be cited.
  A human decision through `brain memory proposals` is the smallest thing that
  actually gates. The migration story is that there is none — packs imported
  before quarantine existed stay live and stay discounted by origin, since
  pulling working memories out of retrieval on an upgrade is a worse failure
  than the one quarantine prevents.
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

## Status

**Landed and tested:** phase 0 (all seven items, including the `graph-lite`
default flip), phase 1, 2a, 2c, 2d, 2e, 2f, 2h, 3a, 3b, 3c, 4a, and the audit
half of 4b.

**Open, buildable:** the BrainBench half of 4c (a MemSyco-derived track lives
in `kimetsu-bench`) and all of phase 5. The render half of 4c has landed, as has
4d.

3d and 3e landed, 3e not as written. The plan said to graduate
`broker.proactive_prefetch` to default-on "gated on a measured false-positive
rate from the regret data". There is no such data and there could not be: the
flag's own doc comment had said graduation waits on it since the flag shipped,
and nothing recorded which hook surface an injection came from, so pooling the
history could only compare prefetch against itself. Injections now carry their
surface and `brain policy` reports acceptance per surface, so the flag has a
gate it can actually pass. The default stays off until someone passes it —
flipping it here would be asserting the number rather than measuring it, which
is the thing this RFC keeps refusing to do.

**Open, blocked on an asset rather than a design:**

| Item | Blocker |
|---|---|
| 2b global normalization | Needs a semantic-build benchmark to justify a default. The ONNX Runtime prebuilt was unreachable from the machine this work was done on, so the `embeddings` flavor would not build, so `brain bench` and `brain eval` could not run. |
| 2a flipping RRF on by default | Same. The rule ships selectable and swept; only the default is unmeasured. |
| 2g entailment conflicts | Needs a local NLI cross-encoder — same download path, same blocker. |
| 2i BEAM 10M | Needs the BEAM corpus and a reader API budget. |

Each of those is a measurement or an asset that has to come from a machine that
can reach the model CDN, not a question about the design.

## What this RFC does not claim

- **The ranking changes are not measured.** 2a ships RRF selectable and swept
  but defaulted off, and 2b is deferred outright, because both need a
  semantic-build benchmark to justify a default and neither has one yet. The
  sweep exists so a corpus can settle it; an argument from the literature is not
  a measurement on your brain.
- Target numbers in phase 2 are targets, not results. Nothing here has been
  re-benchmarked on BEAM or LongMemEval since the changes landed.
- The `graph-lite` default is justified by a *property*, not a benchmark: on a
  corpus with edges present, graph-lite returns a superset of flat's candidates
  with every flat candidate's relevance unchanged, so it can broaden recall but
  never displace a result. That is asserted by a test. The 73.3% vs 62.3% gap
  that motivated it is the pre-existing BEAM measurement.
- Competitor figures cited here are self-reported unless otherwise noted, and
  the 2026 roundups show several of them not reproducing. Ours ship with the
  harness so they can be checked.
