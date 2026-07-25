# Changelog

All notable changes to kimetsu land here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [SemVer](https://semver.org/spec/v2.0.0.html). From v1.0.0
onward the project follows SemVer normally: patch releases are
bug-fix-only, minor releases are backward-compatible additions, and
breaking changes require a major bump.

## Unreleased: Speak first on every host

The README says Kimetsu "speaks first." An audit found that true on Claude Code,
thin on Codex, and absent on Cursor, Pi and OpenClaw. This closes that gap and
files the v3.0 plan behind it.

### Added

- **Injected memory is framed as a prior conclusion, not as knowledge.**
  MemSyco-Bench reports most memory systems scoring *worse* on sycophancy than
  using no memory at all: a retrieved memory arrives looking like ground truth,
  so the model defers to it over evidence directly in front of it. Kimetsu's
  header read "Kimetsu brain relevant knowledge for this task" — and *knowledge*
  is the wrong word for what was recorded at some past moment by someone who
  believed it then. The injection now states which questions memory settles
  (what was decided, learned, or preferred — the repository does not record why)
  and which it does not (what the code currently is), and resolves the conflict
  case explicitly: prefer what you can check now over what was recorded then.
  The memory itself is never hedged — an agent told "this might be wrong"
  ignores memory, which is the whole product — only outranked. The proactive
  hook gets a short suffix rather than a preamble, since it interrupts work
  already underway on a one-capsule budget. MCP clients get the same rule
  through `usage.how_to_use`.

- **`brain import` quarantines packs from the network.** Provenance-weighted
  trust discounts a poisoned pack; it does not stop one. Memory poisoning is
  worth stopping rather than ranking down precisely because, unlike prompt
  injection, it persists across every future session. So an imported pack is now
  held in the review queue `brain memory proposals` already drives, and nothing
  in it can reach a session until you accept it. On by default for `http(s)://`
  sources — content authored elsewhere and fetched over the network is the widest
  attack surface here — and off for a local file or stdin, which you chose and
  can open; `--quarantine` / `--no-quarantine` override either way. `--mode
  replace` alongside quarantine is refused, since superseding memories you have
  in favour of content you have not read is the worst of both. Entries matching a
  memory you already hold are deduped rather than proposed, because a review
  queue nobody can face is not a safety mechanism. Nothing reaches backward:
  packs imported before this stay live and stay discounted by origin.

### Fixed

- **The skills loop tells you it is waiting.** A memory cited across three
  distinct runs earns skill status, and detection has run on the maintenance
  schedule since the daemon landed — into a log file nobody opens. So a memory
  could cross the threshold and sit there indefinitely, because closing the loop
  needed a person who was never told there was anything to close.
  `find_synthesis_candidates` having exactly one caller was the same fact stated
  as a call graph. The warm start now carries one line naming what is waiting and
  the command that acts on it: candidates with no draft yet, and drafts awaiting
  a decision. A candidate that already has a proposal is not counted as a
  candidate — the loop has moved on. Silent when there is nothing to act on.
- **Proactive injections are scored per hook surface.** `broker.proactive_prefetch`
  has been default-off since it shipped, its doc comment saying graduation waits
  on evidence that file-path-augmented queries do not add noise — while nothing
  recorded which surface an injection came from, so that evidence could never
  accumulate and the flag could never graduate on any timescale. Injections now
  carry their surface, and `kimetsu brain policy` reports acceptance (injected
  vs. subsequently cited) for each: `posttool`, which reacts to a command that
  observably failed; `pretool_command`; and `pretool_prefetch`, which predicts
  from a file path alone. Scored apart, so a strong surface cannot launder a weak
  one's noise. Events from before this change carry no surface and are dropped
  rather than bucketed into a default, which would credit one surface with
  another's history. The default stays off until the comparison is made on a real
  brain — what changed is that it can be.

- **Ordering questions get dated, ordered memories.** Memories carry
  `created_at` and capsules did not, so a bundle rendered in score order with no
  dates gave a reader asked "did we switch to `thiserror` before or after the
  migration?" nothing to order the answer by — the reason event ordering was the
  weakest measured ability (32.5% on BEAM 100K). Retrieval was never at fault:
  the memories were found and selected, and the ordering was thrown away at
  render time. When a query carries an ordering marker (`before`, `after`,
  `first`, `when`, `timeline`, …) the bundle is now re-rendered oldest-first with
  each memory's date in front of its text, under a line telling the reader that
  is what they are looking at. It runs after the budget loop, so it is
  presentation and not selection — it cannot admit a capsule the broker rejected
  or drop one it chose — and `used_tokens` is recomputed because the dates cost
  tokens. Repo files, which have no position in the memory timeline, keep their
  relative order after the dated ones rather than being dropped or dated. The
  marker gate is narrow on purpose: dating every capsule on every query would
  spend tokens on the majority of questions that are not about time. Exposed as
  `chronological` / `chronological_note` on `kimetsu_brain_context`.

- **Pi and OpenClaw now inject anything at all.** Both generated extensions
  spawned `kimetsu` with `stdio: "ignore"`, so the context hook read an empty
  stdin (bailing on its minimum-prompt guard) and its output went to
  `/dev/null`. Kimetsu was write-only on those hosts. The extensions now pipe
  stdio, feed the hook payload in, and return the parsed context through the
  host's own contract — Pi's `before_agent_start` message, OpenClaw's
  `agent_turn_prepare` `prependContext`. Every failure mode stays a silent
  no-op: missing binary, hung binary, crash, unparseable output.
- **One source of truth for integration assets.** The Pi and OpenClaw templates
  moved out of string literals in `bridge.rs` into
  `crates/kimetsu-chat/assets/`, pulled in with `include_str!`. The published
  `kimetsu-pi` npm copy is vendored from the same file by
  `scripts/sync-pi-package.sh`, and a new CI job diffs the two. The drift this
  prevents was real: the npm copy had timeout hardening the installed copy
  never got, so a hung binary could stall a Pi lifecycle hook indefinitely.
- **`digest::is_stale()` is wired up.** It was implemented and documented as
  driving a detached rebuild, with no production caller — so a moved corpus put
  a synchronous digest rebuild in front of the agent's first turn. The cached
  digest is now served immediately and the rebuild spawns detached.

### Added

- **Warm start on every host.** `kimetsu brain context-hook
  --warm-on-first-prompt` prepends the repo digest and episodic resume to the
  first turn of a session, exactly once — covering Codex (which has no
  `SessionStart` event), Pi, OpenClaw, and any future host with only a per-turn
  hook. Claude Code does not pass the flag and keeps its `SessionStart` route,
  so nobody receives the block twice. A short prompt still gets the warm start:
  the length guard exists to skip meaningless retrieval, not to withhold
  orientation.
- **Cursor warm start over MCP.** Cursor has no hook surface at all, so the
  first `kimetsu_brain_context` call of a session now returns a `warm_start`
  field alongside the capsules, and the installed Cursor rule tells the agent to
  make that call at task start. Scoped to the stdio path only — one
  `kimetsu-remote` process fans out across many sessions.
- **`docs/rfcs/v3-proactive-memory.md`**: the v3.0 RFC. What the audit found
  (including where the numbers and the defaults disagree), the 2026 competitive
  and research landscape, the Free/Deep two-tier split, and the phased plan for
  retrieval accuracy, proactive autonomy, and memory safety.

### Docs

- `interfaces.md` listed `kimetsu_skill`, `cite_memory` and `expand_capsule` as
  MCP tools. The first does not exist; the other two belong to the autonomous
  agent pipeline, and are now documented there instead.
- `the-broker.md` documents that capsules fill against **half** of the requested
  `budget_tokens` — every published budget number read as double the real one.
- `[storage] backend` no longer carries stale TODOs for `graph-lite` and
  `graph`; both are implemented, and the doc now says what they do and notes
  that `graph-lite` needs `kimetsu brain graph build` before it differs from
  `flat` at all.

## v2.5.3: Close the benchmark learning loop

A focused follow-up to v2.5.2. Adds the host-side path that lets a graded
task feed its outcome back into the brain, so the consolidation machinery
(usefulness, query-routes, staples) receives a real signal on every solve.

### Added

- `kimetsu brain benchmark-credit --task <t> --passed`: on a pass, groups a
  citation for the memories most relevant to the task and routes it through
  the same grouped, query-linked path consolidation consumes. Driven by a
  benchmark harness after grading, so the learning signal never depends on
  the agent inside the sandbox calling a tool. This is what makes an
  iterated-benchmark learning loop actually reach the brain; before it, the
  benchmark memory path recorded outcomes but never exercised consolidation.

## v2.5.2: The brain that learns from its own outcomes

Consolidation v1: the first release where citation outcomes reshape the brain
itself, with every mechanism model-free and every constant documented on one
page. Backward-compatible (schema v9 → v10, automatic on open).

### Consolidation v1 (new)

- `kimetsu brain reinforce`: offline consolidation pass. `--staple` merges
  memories that were repeatedly cited together into single fact memories
  (precomputed multi-hop joins; originals kept; provenance carries part ids).
  `--routes` builds a query-routing index from citation history: memories that
  answered similar questions before get a bounded retrieval boost.
- `kimetsu brain cite` accepts repeated `--memory-id` (citing a GROUP — the
  co-citation signal stapling consumes) and `--query` (linking citations to
  the question they answered, feeding the routing index).
- Retrieval-time routing boost with a fixed per-retrieval budget, per-candidate
  cap, minimum support, and power-law decay.

### Learning-loop guardrails

- The usefulness boost's relevance gain is capped: a heavily-cited memory can
  win ties but can never outrank a genuinely more relevant one (fixes a
  measured retrieval-flooding failure mode under sustained citation feedback).
- Cited-in-failure penalty now scales with citation history
  (`-1.0 / (1 + prior citations / 3)`): a proven memory absorbs occasional
  flaky-run hits; an unproven one takes the full penalty.
- All scoring constants (deltas, caps, envelopes, decay) now live in one
  documented module, `scoring.rs`.

### Quality of life

- Schema-mismatch errors name the offending `project.toml` and hint at the
  fix (project discovery climbs to the enclosing git root, so the file is
  often far from where you ran the command).
- npm packages ship with provenance attestations (Sigstore): every published
  package is publicly verifiable as built by CI from this repo.
- Internal: `project.rs` and the CLI `main.rs` split into focused modules.

### Benchmarks

- LoCoMo: 89.4% on the standard 1,540-question set (LLM-judged accuracy).
- Learning-curve harness validated: citation feedback never degrades
  retrieval across repeated iterations, with a train/holdout split proving
  generalization.

## v2.5.0: Ship the brain to your team

Flagships: retrieval level presets (`basic`/`flexible`/`deep`/`advanced`),
temporal validity and date-aware ingestion, fleet write-safety (concurrent
writers, per-event origin), convergent team sync (HLC replay + conflict
surfacing), Kimetsu Remote GA with per-user attribution, and shareable brain
packs (gzip + security-scrubbed export, merge/replace import from file or
URL). Docs site moved to kimetsu.dev.

## v2.0.0: Never explore twice

The biggest token sink is RE-EXPLORATION: the agent re-deriving what the brain
(or the repo) already knows. v2.0 attacks it from three flagship directions and
adds a pluggable storage tier. Backward-compatible: existing `project.toml` and
`brain.db` files upgrade in place (schema v3 → v6, automatic on open).

### Flagship: Session warm-start (never re-establish your work)

ADDED
  * **Episodic work-resume.** A new `work_episode` record (event-sourced,
    schema v5) captures your working state at SessionEnd: the task, what you
    did, what FAILED and why (dead-ends), open threads, and the working
    hypothesis, scoped per-repo (one live episode per repo). `kimetsu resume`
    prints it; `kimetsu checkpoint [note]` saves one manually. Distilled via
    the cheap model when configured, else a rule-based summary (never blocks).
    Episodes are LOCAL-ONLY and never sync/export.
  * **Project digest.** `kimetsu brain digest [--refresh]` assembles a compact
    (~400-token) digest: repo manifest + top-usefulness memories + current
    focus, cached at `.kimetsu/digest.md` by content hash, refreshed on git/
    corpus drift. (The digest is currently rule-based; a cheap-model
    distillation hook-point is present but not yet wired.)
  * **SessionStart injection.** A new `kimetsu brain session-start-hook` emits
    the digest + resume as `additionalContext`, gated by `[broker] warm_start`
    (default on). Wired for Claude Code; other hosts' SessionStart context
    surface is not yet verified and is intentionally left unwired.

### Flagship: The active brain (never wait on the brain)

ADDED
  * **`kimetsu brain ask "<question>"`.** Terminal Q&A answered entirely from
    the brain: full retrieval + a grounded answer composed by a LOCAL/cheap
    model, with memory-id citations. Zero frontier tokens; works offline. Falls
    back to distiller credentials, then to verbatim top-capsules when no model
    is configured. Grounded-only: refuses rather than hallucinating when memory
    has no answer. `--json`; an answer marked helpful counts like a citation.
  * **`kimetsu_brain_answer` MCP tool.** A read-only synthesis tool the host
    agent can call mid-task ("what do we know about X?") for a grounded, cited
    brief instead of re-discovering.
  * **Command recall fast-path.** "How do I…" queries surface matching
    `command`-kind memories runnable-line-first.
  * **Answer-grade injection.** Very-high-confidence capsules (score ≥
    `[broker] answer_grade_min_score`, default 0.92) are prefixed "Verified
    answer from project memory:" so the model can act in one turn. Render-time
    only (ranking is untouched) and suppressed when the memory was recently a
    floor-drop regret.
  * **Proactive pre-fetch.** Opt-in `[broker] proactive_prefetch` (default off)
    warms trajectory-relevant memories at PreToolUse.

### Flagship: Memory → skill synthesis (never re-derive a solution)

ADDED
  * **Skill synthesis.** A memory cited ≥3 times (or a tight cluster) becomes a
    synthesis candidate; `kimetsu brain skills [--review]` drafts an executable
    skill from it (grounded strictly in the cited memories) and, on explicit
    accept, installs it into the host-native skill dir with provenance back to
    the source memory ids. Propose-only (never auto-installed); flagged stale
    when a source memory is superseded. Schema v6 (`skill_proposals`).

### Local-model independence

ADDED
  * **Ollama as a first-class provider** (`provider = "ollama"`, OpenAI-compat,
    `localhost:11434/v1` default, no key) and a single optional `[cheap_model]`
    config that all cheap-model consumers resolve (distiller, consolidation,
    digest, resume, skill draft, `ask`). Back-compatible with
    `[learning.distiller]`; OPTIONAL everywhere: every consumer degrades
    gracefully when no model is configured. `kimetsu doctor` probes the local
    endpoint. New guide: `docs/LOCAL-MODELS.md` (fully-local = zero external
    calls).

### Pluggable storage backends

ADDED
  * **`RetrievalBackend` trait** with `[storage] backend = "flat" | "graph-lite"
    | "graph"` (default `flat`). The broker stays backend-agnostic; switching
    backends re-projects from the event log.
  * **Graph-lite (Tier 1)**: a typed-edge projection (`memory_edges`, schema
    v4) with 1-2 hop expansion blended as graph-provenance candidates (a strict
    superset of flat, no recall loss; the broker still filters). `supersedes`
    edges populate now; episode-sourced edge types are reserved.
  * **Petgraph (Tier 2, remote-only)** behind the `graph` feature (off in local
    lean/embeddings builds, petgraph is never compiled there). In-memory graph
    with centrality / shortest-path / community-detection helpers, plus a
    cross-backend benchmark harness. Spike verdict: an embedded graph DB
    (Kùzu/Cozo) is not justified through ~100k memories.

### Continuous self-tuning + ROI v2

ADDED
  * **Re-tune triggers** (≥50 new memories since last tune, or elevated regret
    rate) surfaced via `kimetsu brain tune --status` + a Stop-hook one-liner,
    proposed, never auto-applied. **Model re-selection advisor**
    (`tune --models`) with reindex/download cost stated. **Regret-driven
    objective**: the tune objective now penalizes floor configs that produced
    regrets. **ROI v2**: per-memory ROI (`roi --top`), output-token accounting,
    and warm-start (`digest_served`/`resume_served`) savings attribution.

### Personal brain sync

ADDED
  * **Event-log replication** (not file copying). `kimetsu brain sync export
    --since <cursor>` / `import` move durable memory-lifecycle events as
    portable JSONL with per-event idempotency; telemetry, raw queries, and
    local-only episodes are excluded by an allowlist; redaction is respected.
    `[sync] dir` drives a server-less directory protocol (Dropbox/Syncthing/NAS)
    with per-machine batches and per-source cursors; `--dry-run` and
    `--status` throughout. Object-storage backends (e.g. S3) are deferred.

### Hardening & release

FIXED
  * Analytics/doctor active counts, reindex, peek/undo, and `memory list` now
    consistently exclude superseded rows; `config set`/`tune --apply` preserve
    toml comments + unknown keys (`toml_edit`); dropped-capsule + proactive-state
    sidecars write atomically. Cursor/Gemini MCP schemas verified against live
    docs.
  * **Release pipeline**: a `version-guard` job fails in seconds on a
    tag/workspace-version mismatch (and the built binary must self-report the
    tag): closing the gap that produced the v1.5.0 botch; core GitHub Actions
    bumped to Node-24-ready versions; `scripts/bump-version.sh` codifies the
    one-step version bump.

KNOWN LIMITATIONS
  * SessionStart warm-start is wired for Claude Code only; the digest is
    rule-based pending the cheap-model distillation wiring; full retrieval-
    quality and first-turn-token benchmarks require the embeddings + Docker
    environment and were not run in CI; remote-bench process isolation (#23)
    remains open.

## v1.5.1: version-stamp re-release

Identical feature set to v1.5.0. The v1.5.0 artifacts were built before the
workspace version bump, so their binaries self-report `1.0.0` (which also
broke the crates.io publish: `kimetsu-core@1.0.0` already existed). npm
forbids reusing a published version, so the corrected release ships as
v1.5.1. If you installed v1.5.0 from npm or the GitHub release, update.

FIXED
  * Workspace and inter-crate versions stamped correctly (`kimetsu --version`
    now reports the release version; crates.io publish unblocked).

## v1.5.0: pays for itself

ADDED
  * **Telemetry capture.** Raw query text is now stored in `context.served`
    telemetry events when `[learning] store_queries = true` (default; set
    `false` to revert to the pre-v1.5 query-hash-only behavior). A
    `session_id` field is also written when the host provides one (Claude Code
    hooks; absent for hosts that do not emit it). Dropped-capsule sidecar
    (`~/.kimetsu/cache/<hash>/dropped-recent.json`) records capsules that
    were floor-filtered out so that a later citation of one is detected as a
    `retrieval.regret` event, feeding the self-tuning loop. All telemetry
    stays on-machine; nothing is exported.

  * **ROI ledger (`kimetsu brain roi`).** Conservative per-kind token-savings
    estimates (failure_pattern=1500, command=400, convention=300, fact=500,
    preference=200 tokens per citation) minus brain-injection overhead give a
    net-positive / net-negative verdict. Dollar estimates are shown when the
    active model is recognized from a built-in price table (Claude 3/4, GPT-4/5
    families, Bedrock routing prefixes) or when `[model] price_per_mtok` is set
    in `project.toml`. `--window 7d|30d|all` and `--json` for scripting. The
    Stop hook appends a per-session savings sentence when ≥1 citation occurred;
    zero-citation sessions are silent. Calibration methodology and honest
    limitations: `docs/ROI-METHODOLOGY.md`.

  * **Token budget: render-time capsule compression and session dedupe.**
    Two `[broker]` toggles, both default `true`:
    - `compress_capsules`: capsule summaries are compressed at render time
      (strips `[tags: ...]` / `(context: ...)` annotations, caps at 3
      sentences). Ranking is never affected: this runs only after retrieval
      and reranking. Set `false` to inject full memory text.
    - `session_dedupe`: the `UserPromptSubmit` hook skips capsules whose
      handle was already injected earlier in the same session (tracked via
      the proactive-state sidecar). Soft policy: falls back to injecting all
      capsules when dedupe would produce an empty set. This addresses the
      pre-v1.5 behavior where the main hook re-injected the same top capsule
      on every prompt of a long session.

  * **Self-Tuning Brain: `kimetsu_brain_cite` MCP tool + `kimetsu brain tune`.**
    `kimetsu_brain_cite` is a new write-gated MCP tool that records a
    `memory.cited` event from inside an MCP session, closing the ground-truth
    gap when the model leans on a memory but doesn't explicitly call
    `cite_memory`. Personal eval set: `tuneset` builds positive eval cases from
    `context.served` + citation joins (exact session_id or ±30-minute window
    fallback). `kimetsu brain tune --status` reports case count and kind
    coverage. `kimetsu brain tune` (dry-run by default) sweeps `broker.min_lexical_coverage`
    ∈ {0.3, 0.4, 0.5, 0.6} × `broker.min_semantic_score` ∈ {-1.0(AUTO), 0.0, 0.25,
    0.35, 0.45} against the production embedder; `--apply` writes only the
    floor parameters (not the reranker; that change is recommended separately);
    `--revert` restores the previous tune-history entry. A holdout guardrail
    (deterministic 20% split) prevents writing a config that regresses the
    holdout objective.

  * **Consolidation: `kimetsu brain consolidate` + `kimetsu brain triage`.**
    Schema migrated to **v3** (`superseded_by` column + index on `memories`).
    `brain consolidate` (Story 3.1, default): brute-force cosine scan within the
    same embedding model; clusters at ≥ 0.92 cosine (configurable with
    `--threshold`) are merged: survivor keeps its id and text, members get
    `superseded_by` set and a `memory.superseded` event written (rebuild-safe);
    citations are reassigned to the survivor. `--distill` (Story 3.2): looser
    clusters (0.75-0.85 cosine band, ≥3 memories, ≥1 shared tag) are fed to
    the configured distiller; result lands as a memory proposal for human
    review; prints clusters and exits 0 when no distiller is configured.
    `brain triage` (Story 3.3): interactive per-item keep / prune / skip of
    memories below a usefulness and age threshold (`--score-floor 0.2`,
    `--age-days 30`); `--prune-all --yes` for batch non-interactive pruning.

  * **Reach: export redact, Cursor + Gemini CLI installers, CI embeddings job.**
    `kimetsu brain export --redact` strips the `(context: …)` segment from
    exported memory text; `--redact-tags` (requires `--redact`) additionally
    strips the `[tags: …]` prefix. Both flags are useful for sharing brains
    without leaking workspace-specific file paths or tag metadata.
    `kimetsu plugin install cursor` and `kimetsu plugin install gemini-cli`
    write MCP config (`.cursor/mcp.json` / `.gemini/settings.json`) and an
    always-on guidance file (`.cursor/rules/kimetsu-brain/rule.md` for workspace
    installs; `GEMINI.md` merged into the project root or `~/.gemini/GEMINI.md`
    for global installs). Neither host has a `UserPromptSubmit`-style hook
    system, so MCP + the guidance file are the complete integration surface.
    The Cursor and Gemini CLI config schemas match each host's current official
    MCP documentation (Cursor: `mcpServers` with `type: "stdio"`; Gemini CLI:
    `mcpServers` with transport inferred). CI: a new `test-embeddings` job (ubuntu-only,
    `--features embeddings`, HuggingFace + fastembed cache) runs alongside the
    existing lean test matrix.

CHANGED
  * `kimetsu.mcp_write_tools` gate now covers `kimetsu_brain_cite` alongside
    the existing write tools (same env / config / default-true logic for the
    local stdio server; remote server remains env-only, default-deny).
  * Stop hook output includes a per-session token-savings line when the brain
    was cited at least once that session.
  * `brain.db` schema advanced from **v2 → v3** (automatic on first
    read-write open; sidecar backup taken per the existing migration policy).
  * `brain export` gains `--redact` / `--redact-tags` flags (no behavior
    change for existing callers).

FIXED
  * Tune sweep now runs against the production embedder (not the Noop embedder
    used in tests), so floor calibration is on real vectors.

## v1.0.0: durable migrations, analytics, semantic retrieval, proactive recall

ADDED
  * **Remote cross-encoder rerank stage.** `kimetsu-remote serve` now applies a
    cross-encoder reranker to every `kimetsu_brain_context` call (`--reranker`,
    default `jina-reranker-v1-tiny-en`, operator-level; `"off"` disables; any
    curated or HuggingFace ONNX id accepted). The default was chosen by the
    100-memory benchmark: jina-tiny MRR 0.931 vs 0.914 for TinyBERT on the local
    bench; the remote path has no hook-latency budget so the fastest effective
    reranker wins. Benchmark lift with reranker: jina-v2-base-code MRR 0.904 →
    0.906, bge-small MRR 0.901 → 0.909 (production floors active).

  * **Model-aware AUTO semantic floor + kimetsu-remote benchmark.** `kimetsu
    brain bench --remote` boots a real kimetsu-remote server per embedder,
    drives the 100-case dataset over HTTP MCP (sequential + concurrent), and
    reports quality/latency/throughput/server-RSS. Its first run caught a
    real bug: the 0.35 cosine floor (calibrated on bge) was KILLING relevant
    jina-v2 results on every production path (remote MRR 0.90 -> 0.77).
    `broker.min_semantic_score` now defaults to -1.0 = AUTO: 0.35 on
    bge-family models, disabled elsewhere (jina-v2 own precision keeps noise
    low); explicit values still win. Confirmed: jina-v2 remote recovered to
    MRR 0.906 / recall@4 0.939 on the 100-memory corpus (with the server
    reranker: ~416ms/request, ~5 rps at concurrency 4, ~1.2GB peak RSS).
  * **Benchmark-chosen retrieval defaults: jina-v2-base-code + TinyBERT.**
    `kimetsu brain bench` (100 real-memory cases) drove the defaults:
    embedder `jina-v2-base-code` (recovers oblique queries bge-small never
    pools; ~4x less off-topic noise) + reranker `ms-marco-tinybert-l-2-v2`
    (~43ms, within noise of the best MRR). Existing brains need
    `kimetsu brain reindex` after upgrading (vector dims 384 -> 768); set
    `embedder.model`/`embedder.reranker` back to taste and re-judge with
    `kimetsu brain bench`. Lean-RAM alternative: bge-small + tinybert.
  * **Local MCP write tools enabled by default (`kimetsu.mcp_write_tools`).**
    The brain's own workflow tells the agent to record lessons
    (`kimetsu_brain_record`, the Stop-hook harvest cue), but the privileged-
    write gate default-denied unless `KIMETSU_MCP_ENABLE_WRITE_TOOLS=1` was
    in the MCP server's env, so every session ended with the agent goaded
    into a blocked call. The gate is now config-driven for the LOCAL stdio
    server: `kimetsu.mcp_write_tools` (default true), personalizable via
    `kimetsu config set kimetsu.mcp_write_tools false`. Precedence: the env
    var when set always wins (both directions) > config > default. The
    REMOTE server is unchanged (env-only, default-deny) because a cloned
    repo's project.toml is untrusted input and must never enable writes.
  * **Cross-encoder reranking (opt-in) + retrieval eval harness + 300ms
    hook budget.** The warm daemon can apply a final cross-encoder rerank
    stage: over-fetch a 12-capsule pool, score each (query, memory) pair
    jointly with a fastembed reranker (`[embedder] reranker` = a curated id;
    local default ms-marco-tinybert-l-2-v2; "off" disables), drop below a 0.30 relevance floor, truncate to the cap.
    Every knob was chosen by measurement: `jina-reranker-v1-tiny-en`
    (default) beat the larger turbo model on both quality and speed in the
    head-to-head benchmark, a pool of 6 matches pool-12 quality exactly at
    half the latency, and summaries must stay FULL (snippet truncation
    cratered recall@4 from 0.83 to 0.66, worse than FTS). With those
    settings a warm rerank answers inside the 300ms hook budget on a real
    brain (~265ms measured); slower machines degrade gracefully to
    floored-FTS for that turn, or set `reranker = "off"`. Backing the
    default path, the cosine floor: `broker.min_semantic_score` is
    now ON by default (0.35) *and actually wired* (it previously defaulted
    to 0.0 and was never populated from config in any production path). The
    hook's daemon budget tightens 750ms → 300ms; a warm semantic answer
    fits with ~70ms to spare, and misses fall back to floored FTS. Daemon
    spawn hygiene for the lazy path: the entrypoint now binds the socket
    BEFORE loading models (a redundant spawn exits in ms, not seconds), and
    on Windows the hook clears HANDLE_FLAG_INHERIT on its std handles before
    spawning so the long-lived daemon can never hold the harness's stdout
    pipe open (previously the first prompt of a session could stall until
    the host's hook timeout). New `kimetsu brain eval [--fixture ...]`
    measures recall@2/4 + MRR across fts / semantic / semantic+rerank modes
    against a committed fixture (`fixtures/eval-retrieval.json`), so ranking
    changes are measurable: baseline shows semantic recall@4 0.90 / MRR 0.91
    vs FTS 0.72 / 0.81. `--rerankers <ids>` benchmarks cross-encoders
    head-to-head (quality + per-query latency) and `--pool` sweeps pool
    sizes; non-curated models load as user-defined ONNX from any HuggingFace
    repo via hf-hub. Benchmark results: jina-tiny recall@4 0.896 / MRR 0.938
    at ~44ms per query (pool 6) vs turbo 0.833 / 0.875 at ~137ms (pool 12);
    ms-marco TinyBERT-L-2 is ~5× faster still (8.5ms) but its quality
    (0.715) merely matches FTS.
  * **Warm embedder daemon: semantic recall at hook time.** The
    `UserPromptSubmit` context-hook can now match memories by *meaning*, not
    just lexically. A single per-user daemon (`kimetsu brain embed-daemon`,
    keyed by embedder model) loads the ONNX model once and serves full
    embedding/ANN retrieval to the hook over a local socket / named pipe
    (`interprocess`); the hook is a thin client with a ≤300ms budget and a
    hard fall-back to floored-FTS, so the prompt is never blocked. `kimetsu
    brain warm` (wired to each harness's startup hook) pre-warms it so a
    running session never pays a cold model load. One model in RAM regardless
    of how many projects/agents are active. Config: `[embedder] daemon` and
    `warm_on_start` toggle it; `[embedder] model` (and `kimetsu brain model
    set`) pick the model. `KIMETSU_EMBED_DAEMON=0` is a runtime kill switch.
    Embeddings builds only; lean builds keep the floored-FTS hook. New
    `kimetsu brain daemon status|stop` to inspect/control it.
  * **Durable schema migrations.** brain.db now migrates forward
    automatically on open via a versioned, forward-only runner (each
    migration applied in one transaction). The DB is backed up to a
    `brain.db.bak-*` sidecar before any version-advancing migration
    (skipped for empty brains; the three newest backups are kept).
    Read-only opens of an un-migrated brain degrade gracefully; an
    event-upcast seam keeps old traces replayable. The project.toml
    config version is decoupled from the DB schema version so the
    schema can evolve without breaking existing projects.
  * **Proof-of-value analytics.** New `kimetsu brain insights` command
    and `kimetsu_brain_insights` MCP tool: retrieval hit-rate &
    skip-rate, citation rate, proposal acceptance rate, usefulness
    trend, harvest yield, corpus health, and token economy, computed
    over a configurable recent-runs window. A new `context.served`
    event records every retrieval (hit or miss); `context.injected`
    now carries injected-token counts.
  * **Semantic retrieval (usearch HNSW ANN).** On the embeddings build, an
    approximate-nearest-neighbour index (usearch HNSW) finds memories whose
    *meaning* matches the query even with no shared words: **O(log N) per
    query**, so retrieval stays fast as the corpus grows. The index is
    candidate generation only; final ranking is an exact cosine rerank over the
    stored f32 vectors, so the index can be quantized (**f16 by default**,
    `i8`/`f32` via `KIMETSU_ANN_QUANTIZATION`) for a large RAM saving with
    negligible quality loss. Retrieval is sharpened with embedding-MMR
    (collapses true paraphrase duplicates) and an absolute semantic-relevance
    floor (genuinely off-topic queries return nothing). Capsule caps are
    config-driven (default 8) and injected tokens drop while the relevant
    capsule is preserved. The lean (FTS-only) build is unchanged.
  * **Scales to ~1M memories.** A million-memory corpus runs on modest
    hardware: **~1.8 s p99 semantic retrieval and ~3 GB RAM at 1M** (f16
    default; ~2.8 GB with `KIMETSU_ANN_QUANTIZATION=i8`). Both retrieval *and*
    conflict-detection-on-write are O(log N) via the HNSW index, no
    brute-force vector scan. Bulk ingest batches embedding; the index builds in
    parallel across cores, maintains itself incrementally, persists a sidecar
    so a restarted server loads instead of rebuilding, and Kimetsu Remote
    pre-warms each repo's index on startup.
  * **Proactive & cost-shrinking recall (the agent brain).** Before the
    first implementation attempt, a tight retrieval surfaces a "Known
    pitfalls" block (failure patterns / conventions), proactive, not
    just post-failure. Tasks are classified (Debug / Feature / Refactor /
    Docs / Investigation) to route recall by kind. A per-run recall
    ledger deduplicates capsules across stages (rendered once,
    back-referenced after), and the long tail is injected as one-line
    headlines the agent expands on demand via a new `expand_capsule`
    tool, so brain overhead shrinks in relative terms as tasks grow
    (an adaptive sublinear, per-run-capped budget).
  * **`kimetsu config edit` and `kimetsu run abort`** are now fully
    implemented: `config edit` opens `$EDITOR` on project.toml and
    re-validates on save; `run abort` cleanly finalizes a dangling run.
    No stub subcommands ship.
  * **`kimetsu doctor --selftest`** proves the brain pipeline works
    end-to-end (ingest → retrieve → record) without needing a live
    model or network.
  * **Process & maintenance commands.** `kimetsu ps` / `stop` /
    `restart` list and stop running MCP servers (the host respawns one
    on the next tool call); `doctor` now flags a stale running MCP
    server after an update. `kimetsu brain export` / `import` move
    memories between brains as portable JSON; `kimetsu brain memory
    edit` / `undo` fix a bad recording in place; `kimetsu runs prune`
    and `kimetsu brain compact` (VACUUM, optional event-trim) keep the
    install lean.
  * **Kimetsu Remote (beta): the brain over HTTP MCP.** Under active testing;
    the `kimetsu-remote` **server is a separate package** and is NOT installed by
    `cargo install kimetsu-cli` / `npm i -g kimetsu-ai`: install it on the
    server with `npm install -g kimetsu-remote` or `cargo install kimetsu-remote
    --features embeddings` (or its standalone GitHub-Release archive). A new
    standalone `kimetsu-remote` server hosts one brain per repository under a
    data dir and exposes the memory/retrieval/curation tools over remote MCP
    (`POST /mcp/{repo}`), so a team (or you across machines) can share one
    brain with no local checkout. Bearer-token auth (global or per-repo);
    repo-keyed (the client supplies the id, derivable from the git remote);
    the agent-facing pure-DB tool subset only (workdir/host-local tools are
    excluded). Each repo brain is standalone (user-brain merge off). Plain
    HTTP: terminate TLS at a reverse proxy. `kimetsu-remote serve --addr
    0.0.0.0:8787 --data <dir> --token <t>` (build with `--features embeddings`
    for semantic retrieval). Wire a host with `kimetsu plugin install
    <claude-code|openclaw> --remote <url> [--repo <id>] [--token <t>]`: it
    writes a `url`+`Authorization` MCP entry (no local hooks), deriving the repo
    id from your git remote and referencing `${KIMETSU_REMOTE_TOKEN}` by default
    so the secret isn't written to disk. The server ships as a separate package
    (`npm i -g kimetsu-remote` / `cargo install kimetsu-remote --features
    embeddings`) with its own standalone release archive. Hardening: per-token
    rate limiting
    (`--rate-limit <req/min>` → 429 when exceeded), a structured per-request log
    + an unauthenticated `GET /metrics` (Prometheus text, aggregate counts by
    outcome, no repo labels), and optional in-process HTTPS (build
    `--features tls`, pass `--tls-cert`/`--tls-key`; rustls/ring, off by default
    a reverse proxy is still the recommended terminator). Optional shared
    **org brain** (`--org-brain <dir>`, outside `--data`): `global_user`-scoped
    memories are stored there and merged into every repo's retrieval
    (cross-project team memory), while `project`-scoped memories stay per-repo.
    Off by default: each repo brain is standalone. Optional **server-side
    ingest** (`--repos-file` + `--checkout-dir`): the operator pre-registers
    repo-id → git URL, the server clones/refreshes a managed checkout, and
    `kimetsu_brain_ingest_repo` indexes its files into the repo's brain so
    `context` retrieval includes file capsules remotely (clients can't trigger
    arbitrary clones; private repos use the server's own git auth).
  * **AWS Bedrock provider.** The agent *and* the auto-harvester can run
    on Anthropic models served through Amazon Bedrock (InvokeModel,
    SigV4-signed from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
    (+ optional `AWS_SESSION_TOKEN`) and `AWS_REGION`, no AWS SDK). Set
    `[model] provider = "bedrock"` and/or `[learning.distiller]`; the two
    are configured independently, so you can run the agent on Bedrock and
    harvest on Bedrock or direct Claude/OpenAI.
  * **Two more host integrations: Pi and OpenClaw.** `kimetsu plugin install
    pi` wires a TypeScript extension (Pi has no MCP) plus a `kimetsu-brain`
    skill; `kimetsu plugin install openclaw` registers the MCP server, a
    hooks plugin, and a `kimetsu-context` skill. Both join Claude Code and
    Codex across `plugin status`, `plugin uninstall`, and `setup`.
    *Which* hosts you wire is a runtime choice you change anytime, no
    reinstall. **Every official prebuilt + npm binary (lean and embeddings)
    includes all four host integrations;** they're opt-in Cargo features only
    for a minimal source build, added with `--features pi,openclaw`. Every
    embedded hook degrades to a silent no-op if the `kimetsu` binary isn't on
    PATH, so a host is never left broken.
  * **Full plugin lifecycle.** `kimetsu plugin status` shows what's wired
    where (host × scope: installed / partial / absent + which pieces);
    `kimetsu plugin uninstall <host>` removes only the wiring (keeping the
    binary + brain); `kimetsu setup` runs init + plugin install + a
    selftest in one command. `kimetsu brain backup` writes a consistent
    full-DB snapshot via the SQLite online-backup API.
  * A 5-minute quickstart was added to the README.

CHANGED
  * **Lean `.kimetsu/`.** The `brain.db` events table is now the
    durable log: memory writes no longer create per-write `runs/<id>/`
    directories, so a brain-only `.kimetsu/` holds just `brain.db` +
    `project.toml`. Transient proactive / chat / bench output moved to
    `~/.kimetsu/cache/`.
  * **Bidirectional config: every optional feature is turn-off-able.**
    `[embedder] enabled`, `[broker] ambient`, `[kimetsu] use_user_brain`
    (plus the existing `[learning] auto_harvest` / distiller and
    `[shell] redact_secrets`) are honored at runtime with the precedence
    env override > config > default. New `kimetsu config set` / `get`
    read and flip them from the CLI.
  * **Tiered, non-orphaning uninstall.** `kimetsu uninstall` now removes
    the host plugin wiring (Claude Code & Codex hooks / MCP / skills /
    agents, workspace + global) via a 3-tier prompt (binary only /
    + plugins (default) / + brains (typed confirm)) so it no longer
    leaves hosts pointing at a missing binary. A binary locked by a
    running kimetsu process is handled (offer to stop it / deferred
    delete) instead of a misleading "needs admin".
  * **Install/upgrade hardening.** Golden tests lock the
    non-destructive config-merge for Claude/Codex hooks, MCP config,
    and CLAUDE.md (user content always preserved; re-installs are
    byte-idempotent). Windows now runs the full test suite in PR CI.
  * **Clippy is a hard CI gate** (`-D warnings`) on both the lean and
    embeddings builds.
  * **Retrieval ordering is fully deterministic:** a stable tiebreak
    eliminates non-reproducible ranking across runs.
  * **Terser `--help` menu + flavored `--version`.** Top-level commands
    show short imperative labels (full detail stays in
    `kimetsu <cmd> --help`), and `kimetsu --version` reports the build
    flavor (`1.0.0 (embeddings)` vs `(lean)`) so semantic-search
    availability is obvious at a glance.
  * **Run dirs self-prune.** New agent runs opportunistically GC run dirs
    older than 30 days (keeping the newest 20; `KIMETSU_RUNS_GC=0` to
    disable), only at run creation, never on the hot brain-open path.
  * **One-command npm semantic build.** `kimetsu npm-flavor embeddings`
    fetches the semantic build once and persists the choice (in
    `<cache>/kimetsu/npm/flavor`), so npm users no longer keep
    `KIMETSU_NPM_FLAVOR` exported across runs; `lean`/`status` round it out
    (the env var stays a per-run override).

FIXED
  * **Lexical retrieval no longer injects off-topic memories that merely
    share the project name.** A broad conceptual prompt ("tell me about
    kimetsu, what's the idea of the repo") surfaced narrow debugging
    war-stories whose only overlap with the query was the corpus-ubiquitous
    token "kimetsu". Root cause: on the FTS-only `UserPromptSubmit` hook path
    there was no relevance floor (the cosine-based `min_semantic_score` is
    inert without an embedding), and `normalize_and_score` divides relevance
    by the *per-kind* max, so the best memory of each kind became
    `relevance = 1.0` no matter how weak the actual match, easily clearing
    the `min_score` gate on freshness + confidence alone. New
    `broker.min_lexical_coverage` floor (default 0.5): query tokens are
    stripped of stopwords and IDF-weighted over the memory corpus (so the
    project name, present in nearly every memory, contributes ~0; a word in
    *no* memory also contributes 0 since it can't discriminate), and a memory
    is dropped before scoring when the IDF-weighted share of the query it
    covers is below the floor and it has no semantic support. Repo-file and
    manifest capsules pass through untouched. Memories whose only match is the
    project name are now reliably dropped; the brain stays silent rather than
    injecting noise. (Keyword-overlap-but-off-topic hits that match a real,
    non-ubiquitous word still need the semantic path; this is a lexical floor.)
  * **`Stop` hook no longer trips "invalid stop hook JSON output".**
    `kimetsu brain stop-hook` printed a bare-text banner on stdout, but
    Claude Code validates a Stop hook's stdout as the advanced JSON
    control object, so the banner was rejected. The hook now emits a
    well-formed JSON object: informational banners via `systemMessage`,
    and the end-of-session harvest cue via `decision: "block"` so the
    cue text actually re-enters the model (plain stdout never reached it
    in a Stop hook, so the cue was previously inert). The one-cue-per-
    session guards keep blocking from looping.
  * **MSRV portability.** A 1.87-only API that violated the declared
    `rust-version = "1.85"` MSRV was replaced with the compatible
    1.85 equivalent.
  * **GlobalUser memory writes work from any directory again:** a
    regression where recording a global-user memory required a loadable
    project is fixed.
  * **`UserPromptSubmit` context-hook no longer risks the host's 30s
    timeout.** The per-prompt hook runs in a throwaway process that
    can't reuse the long-lived MCP server's warm model cache, so in the
    embeddings build it was paying a cold fastembed/ONNX model load on
    every prompt, fast on a warm OS file cache but able to exceed 30s
    on a cold first prompt (worst under disk contention / AV scanning),
    which fails the hook. The hook is now FTS-only (lexical retrieval,
    no embedding model loaded); semantic ANN recall stays with the warm
    MCP `kimetsu_brain_context` tool the agent calls. Steady-state hook
    latency drops to ~300 ms regardless of build flavor.

## v0.9.0: auto-harvested memories + SessionEnd distiller

ADDED
  * **Credentialed SessionEnd distiller (opt-in).** A second, deterministic
    memory-harvest path alongside the v0.9.0 in-agent harvester. `kimetsu plugin
    install claude-code` and `kimetsu plugin install codex` now run an
    interactive wizard (on a TTY; skip with `--no-setup`, force with
    `--setup-harvest`) that configures a cheap distiller
    model: Anthropic (recommended `claude-haiku-4-5`), OpenAI (recommended
    `gpt-5.4-mini`), or compatible endpoints via `ANTHROPIC_BASE_URL` /
    `OPENAI_BASE_URL`. The key + base URL are written to a gitignored `.env`;
    the selection lands in `[learning.distiller]`
    in `project.toml`. A new `SessionEnd` hook for Claude Code runs
    `kimetsu brain session-end-hook`; Codex uses its supported `Stop` hook with
    `--distill-on-stop`. When enabled + credentialed, the distiller reads the
    transcript with that model and records lessons through the confidence-gated
    `propose_or_merge_memory`. When the distiller is enabled, the Stop hook's
    end-of-session cue is suppressed (the distiller owns end-of-session; the
    mid-session PostToolUse resolved-failure cue stays). `AnthropicProvider`
    gained an optional base URL for the LiteLLM case, and the distiller gained
    an OpenAI Responses API provider. With `--scope global` the
    wizard configures the distiller once in `~/.kimetsu/` (config + `.env`); that
    global distiller then runs in every project and records into the user brain
    (`~/.kimetsu/brain.db`, available everywhere), with a per-project distiller
    taking precedence when one is configured.
  * **Auto-harvested memories (in-agent).** The hooks now drive memory
    *generation*, not just retrieval. When a command that failed earlier in the
    session succeeds (PostToolUse), or a non-trivial session ends with nothing
    recorded (Stop), Kimetsu emits a `[kimetsu-harvest]` cue telling the agent to
    dispatch a new background **`kimetsu-memory-harvester`** subagent (installed
    at `.claude/agents/` for Claude Code and `.codex/agents/` for Codex, pinned
    to a cheap model). The subagent distills 0-3
    generalizable lessons and records them through the confidence-gated
    `kimetsu_brain_record` path: no separate API key or kimetsu-side model
    credentials, billed in-agent at the cheap model's rate, non-blocking. Cues
    are throttled (at most ~once per resolved failure / once per session) and can
    be disabled with `[learning] auto_harvest = false` in `project.toml`.

CHANGED
  * **Cleaner help & tool menus.** Stripped internal version-history prefixes
    (`v0.x:`, `MP-…:`) from `--help` text and the MCP tool/argument descriptions
    so menus read as plain present-tense descriptions. (Internal code comments
    are unchanged.)

FIXED
  * **Stop hook read the wrong field.** The Stop hook counted
    `kimetsu_brain_record` calls from a non-existent inline `transcript` array;
    Claude Code actually passes a `transcript_path` to a JSONL file. It now reads
    the JSONL (with the inline array kept as a fallback), counts both the bare
    and MCP-namespaced (`mcp__kimetsu__kimetsu_brain_record`) tool names, so the
    "lessons recorded" banner and end-of-session cue actually fire. The JSONL is
    now streamed line-by-line so a long session's multi-MB transcript never
    lands in memory at once.
  * **BOM-tolerant install.** `kimetsu plugin install` now strips a leading
    UTF-8 BOM before parsing an existing `settings.json` / `hooks.json` /
    `config.toml` / `.mcp.json`, so a config saved by a BOM-emitting editor
    (e.g. older Windows Notepad) no longer fails with "expected value at line 1
    column 1".
  * **Install polish.** The installer now **merges** Kimetsu's guidance into an
    existing `CLAUDE.md` (workspace `.claude/CLAUDE.md` or the global
    `~/.claude/CLAUDE.md`) inside `<!-- kimetsu:begin -->`/`<!-- kimetsu:end -->`
    markers, appending and upgrading in place, never overwriting the user's
    content. `--force` no longer overwrites `CLAUDE.md` (the whole install is
    idempotent and non-destructive; the flag is retained only for compatibility).
    A `--scope global` on the workspace-only `kimetsu` target warns instead of
    silently doing nothing; `--workspace` is canonicalized leniently so a global
    install doesn't fail on a missing workspace path.

## v0.8.4: non-destructive plugin install + global scope

ADDED
  * **Global plugin install.** `kimetsu plugin install <target> --scope global`
    installs the Kimetsu surface into the user's home for every session
    (`~/.claude/` + `~/.claude.json` (`mcpServers`) for Claude Code, and
    `~/.codex/` for Codex) instead of the workspace. `--scope` defaults to
    `workspace` (the prior behavior). Also exposed as the `scope` argument on
    the `kimetsu_plugin_install` MCP tool.

FIXED
  * **Hook install no longer clobbers existing hooks.** `kimetsu plugin install`
    now *merges* its hooks into existing Claude `settings.json` / Codex
    `hooks.json` instead of replacing them. Hooks you already have, even on the
    same events Kimetsu uses (`UserPromptSubmit`, `PreToolUse`, …), are
    preserved, with Kimetsu's group added alongside. Re-running install is
    idempotent (no duplicate groups) and the MCP config + generated docs refresh
    without requiring `--force`.

## v0.8.3: npm distribution

ADDED
  * **npm distribution.** Kimetsu now publishes to npm: `npm install -g kimetsu-ai`
    installs the prebuilt native binary for your platform, no Rust toolchain
    required. Uses the esbuild/turbo model: per-platform packages
    (`@kimetsu-ai/linux-x64`, `@kimetsu-ai/darwin-x64`, `@kimetsu-ai/darwin-arm64`,
    `@kimetsu-ai/win32-x64`) selected via `optionalDependencies`, with a thin
    `bin/cli.js` launcher that execs the matching binary. No postinstall, so it
    works under `npm install --ignore-scripts`. The semantic build is fetched on
    demand when `KIMETSU_NPM_FLAVOR=embeddings` is set. Published from the
    existing release pipeline (`publish-npm` job, gated on the `PUBLISH_NPM`
    repo variable + `NPM_TOKEN` secret, mirroring the crates.io gate). Sources
    live in [`npm/`](npm/). Installs the `kimetsu` command (also available as
    `kimetsu-ai`).

FIXED
  * **Windows npm package.** The `publish-npm` job now extracts the Windows
    archive with `7z` instead of `unzip` (PowerShell `Compress-Archive` uses
    backslash path separators that `unzip` flattens), and publishing is
    idempotent so a re-run after a partial failure skips already-published
    versions.

## v0.8.1, v0.8.2

Release-engineering releases on the road to the npm channel. crates.io and the
prebuilt binaries shipped as usual in both. npm naming settled on the
`@kimetsu-ai` scope / `kimetsu-ai` package (v0.8.1), and the complete, working
npm packages (including Windows) ship in v0.8.3.

## v0.8.0: proactive recall, selectable embedding model, full MCP control

The release that makes the brain **proactive** and gives the agent (and user)
full control over it from inside Claude Code / Codex.

ADDED
  * **Proactive recall (mid-work).** New `PreToolUse` / `PostToolUse` Bash hooks
    surface a relevant memory *while the agent works*, not just on prompt:
    - after a **failed** Bash command, surface a matching `failure_pattern` /
      `command` fix;
    - **before** a risky Bash command, warn from a matching `failure_pattern`;
    - on a **repeated** failing command (loop), lower the score floor and bypass
      the throttle so help arrives sooner.
    Retrieval is lexical-FTS-only (no embedding-model load), gated by a high
    score floor (0.45; loop 0.35), capped at one capsule, with per-session
    dedupe + a refractory throttle. Token cost stays ~0 (silent when nothing
    qualifies). Wired into both Claude Code (`.claude/settings.json`) and Codex
    (`.codex/hooks.json`) with a `matcher: "Bash"`; opt out with
    `kimetsu plugin install --no-proactive` (or `proactive:false` over MCP).
    Per-session state lives in `<repo>/.kimetsu/proactive/<session_id>.json`
    and is garbage-collected after 7 days.
  * **Selectable embedding model.** New `[embedder]` table in `project.toml`
    (precedence: `KIMETSU_BRAIN_EMBEDDER` env > config > default). Inspect and
    change it with `kimetsu brain model list` / `kimetsu brain model set <id>`
    (the latter writes the config and re-embeds the corpus with the new model).
    Curated built-ins: `bge-small-en-v1.5` (384d, default), `bge-m3` (1024d),
    `jina-v2-base-code` (768d).
  * **Full MCP control surface.** New tools so an agent can manage the brain
    without leaving Claude Code / Codex: `kimetsu_brain_model_list` /
    `kimetsu_brain_model_set` (re-embeds in-process with the new model),
    `kimetsu_brain_reindex`, `kimetsu_brain_memory_search` (FTS over memory
    text), `kimetsu_brain_conflict_resolve`, `kimetsu_brain_prune`, and
    `kimetsu_brain_config_show`. `kimetsu_brain_memory_list` and
    `..._memory_proposals` gained `limit`/`offset` pagination.

CHANGED
  * `reindex` can now run against an explicit embedder
    (`reindex_all_with_embedder`), so a model switch re-embeds with the chosen
    model regardless of the process's cached default embedder.

FIXED
  * **Test isolation.** Tests created project roots under the system temp dir;
    on a machine whose `$HOME` is itself a git repo, `ProjectPaths::discover`
    climbed to `$HOME` and made parallel tests share one `brain.db` +
    `project.lock`. Each test root now gets its own git boundary, so plain
    `cargo test` is hermetic.

## v0.7.2: remove kimetsu-harbor-rs; first crates.io publish of the v0.7 line

Maintenance + distribution release, layered on top of the v0.7.1 security
hardening (path-traversal guards + URL-credential redaction).

REMOVED
  * **`kimetsu-harbor-rs`**: the Terminal-Bench JSON-RPC transport adapter
    and its `kimetsu-harbor-agent` binary. The benchmark harness drives
    Harbor's built-in `claude-code` agent via `--mcp-config` (since the
    v0.5.5 refactor), so the custom transport binary was dead code. No
    published crate depended on it (`kimetsu-cli`, `-chat`, `-agent`,
    `-brain`, `-core` are unaffected); not a breaking change for
    `cargo install kimetsu-cli` users.

CHANGED
  * Release workflow no longer builds or bundles `kimetsu-harbor-agent`.
  * First crates.io publish of the v0.7 series (v0.6.0 / v0.7.0 / v0.7.1
    shipped binaries + GitHub Releases only).

## v0.7.0: semantic dedup, embeddings by default, session hooks

The release that makes knowledge transfer reliable end-to-end:
capture without duplication, retrieve without asking, and surface
what was learned each session.

ADDED
  * **Semantic dedup at capture.** `propose_or_merge_memory` (new in
    `kimetsu-brain`) runs before any memory is written. Exact dups
    short-circuit; near-dups (cosine ≥ 0.85, same scope) merge into
    the existing memory instead of spawning a near-twin. High-confidence
    novel lessons are accepted directly; the rest file as proposals.
    `kimetsu_brain_record` returns which branch was taken
    (`added | merged | duplicate | proposed`). This stops a brain from
    filling with ten rephrasings of the same lesson over a gauntlet.
  * **Embeddings on by default for the CLI.** `cargo install kimetsu-cli`
    now ships `--features embeddings` (fastembed-rs + ONNX, BGE-small-en-v1.5).
    Cosine retrieval, semantic dedup, and conflict detection all light up
    out of the box. Build lean with `--no-default-features`. Library crates
    (`kimetsu-brain`, `kimetsu-chat`) stay lean by default so downstream
    consumers don't inherit the ONNX runtime; only the binary opts in.
  * **`Stop` hook for session summary.** `kimetsu brain stop-hook` walks
    the transcript, counts `kimetsu_brain_record` calls, and prints a
    post-turn banner, confirming captures or nudging when a non-trivial
    session recorded nothing. `kimetsu init` now writes both the
    `UserPromptSubmit` and `Stop` hooks into `.claude/settings.json`.

CHANGED
  * `kimetsu_benchmark_context` shares argument parsing with
    `kimetsu_brain_context` via an extracted `parse_retrieval_args`
    helper: ~50 lines of duplicated stage/budget/ambient handling
    removed. No behavior change for bench callers.

NOTE
  * On Windows the embeddings flavor needs the VS2022 C++ runtime
    (ort prebuilts). The default model (~24 MB) downloads to
    `~/.cache/huggingface/` on first embed call, then caches.

## v0.6.0: zero-overhead knowledge transfer

Retrieval and capture become silent by default and only speak up when
they have something worth saying.

ADDED
  * **`kimetsu_brain_context` zero-overhead contract.** When the brain
    has nothing relevant it returns `skipped: true` and injects nothing,
    so a host agent can call it on every non-trivial task without paying
    a context tax on cold brains.
  * **`kimetsu_brain_record` capture tool.** The host agent's path to
    persisting a concrete lesson (plus 2-5 domain tags) after solving a
    non-obvious problem. Pairs with `kimetsu_brain_context` as the
    intended two-call loop.
  * **`UserPromptSubmit` context hook.** `kimetsu brain context-hook`
    reads the prompt from stdin and injects a context bundle before each
    Claude Code turn, so retrieval happens whether or not the model
    remembers to ask.

## v0.5.5: delete kimetsu_harbor/: harbor refactor arc complete

Final commit of the v0.5.3-v0.5.5 harbor refactor arc. The Python
Harbor adapter + benchmark glue moved to the internal kimetsu-bench
repo in v0.5.4's sibling commit; this commit deletes the orphan
directory from kimetsu and finishes the cleanup.

DELETED FROM THIS REPO
  kimetsu_harbor/                          (entire directory)
    ├── codex_kimetsu_agent.py            (311 LOC, Codex variant
    │                                      that diverged from the
    │                                      canonical adapter)
    ├── kimetsu_agent.py                   (459 LOC, moved to
    │                                      kimetsu-bench/python/)
    ├── smoke_test.py                      (145 LOC, replaced by
    │                                      crates/kimetsu-e2e in
    │                                      v0.5.3)
    ├── kimetsu-mcp-stdio.sh               (1-line shim)
    ├── codex-kimetsu-mcp.wsl.json         (Codex MCP config)
    ├── kimetsu-mcp-required.md            (user-facing setup doc)
    ├── kimetsu-mcp-optional.md            (user-facing setup doc)
    ├── README.md                          (Harbor adapter setup)
    ├── SETUP-WSL.md                       (WSL setup)
    ├── __init__.py                        (package marker)
    └── archive/                           (one-shot scripts +
                                            historical orchestration)

Net: ~900 LOC of glue + ~10 setup docs removed from the user-facing
repo. The functional pieces (kimetsu_agent.py) survive in
kimetsu-bench/python/ where they belong; they're benchmark infra,
not product code.

ALSO REMOVED
  .gitignore: `/kimetsu_harbor/benchmark-logs/` rule (path no
  longer exists; bench logs land in kimetsu-bench/runs/ which
  has its own .gitignore in that repo).
  crates/kimetsu-harbor-rs/Cargo.toml: updated the `_rs` suffix
  comment (it used to explain a collision with kimetsu_harbor/;
  now explains the legacy historical reason).

WHAT SURVIVES IN KIMETSU REPO (unchanged)
  * crates/kimetsu-harbor-rs/: JSON-RPC transport + the
    `kimetsu-harbor-agent` binary. publish = false. The bench
    consumes it as a normal cargo path dep.
  * CI release matrix still builds `kimetsu-harbor-agent` for
    each platform (Linux, macOS-arm64, Windows × lean/embeddings)
    so a future bench operator can grab a prebuilt binary from a
    GH release archive instead of building from source.

THE HARBOR REFACTOR ARC IS COMPLETE
  v0.5.3: Layer 1: in-process e2e suite + CLI smoke (+13 tests,
           all under 1s, no API keys / no Docker).
  v0.5.4: Doc consolidation: HOW-KIMETSU-WORKS.md replaces the
           22-file docs/ sprawl; historical planning + ship docs
           moved to internal kimetsu-bench repo.
  v0.5.5: kimetsu_harbor/ deleted; the Python Harbor adapter is
           now in the internal kimetsu-bench repo alongside the
           Layer 2 orchestrator + driver trait.

THE NEW SHAPE
  Public kimetsu repo:
    docs/HOW-KIMETSU-WORKS.md              one conceptual reference
    crates/                                product code + e2e tests
    crates/kimetsu-harbor-rs/              JSON-RPC transport (publish=false)
    CHANGELOG.md, README.md, LICENSE-*     standard repo metadata
  Internal kimetsu-bench repo:
    src/                                   BenchmarkDriver + kbench CLI
    src/drivers/terminal_bench.rs          first driver (Terminal-Bench)
    python/kimetsu_agent.py                Harbor adapter shim
    docs/history/                          v0.2-v0.5 planning + ship docs
  Pre-push gate: `cargo test --workspace`  (258 tests, ~1s for e2e + cli smoke)
  Impact measurement: `cd bench && cargo run -- --driver tb ...`

VERIFIED
  cargo test --workspace      258 / 258 passing  (unchanged from v0.5.3)
  cargo build --workspace     clean at 0.5.5

UPGRADE NOTES
  * If you had local scripts referencing `kimetsu_harbor/` paths,
    update them to point at the bench repo (or ping for access
    to the private repo).
  * The `kimetsu-harbor-agent` binary at `target/release/kimetsu-harbor-agent`
    is unchanged. Its source still lives at
    `crates/kimetsu-harbor-rs/src/bin/kimetsu-harbor-agent.rs`.

## v0.5.4: doc consolidation: HOW-KIMETSU-WORKS.md replaces the docs/ sprawl

Second commit of the v0.5.3-v0.5.5 harbor refactor arc. Cleans up
`kimetsu/docs/` so users see exactly one conceptual reference, not
22 files of planning, postmortems, and historical roadmaps.

WHAT v0.5.4 ADDS
  * `docs/HOW-KIMETSU-WORKS.md` (~600 lines). Single conceptual
    reference covering: the brain (events → projector → memories),
    the broker (scoring math + decay + MMR), citations + blame
    (v0.5.0), decay (v0.5.1), conflict detection (v0.5.2), the 18
    kimetsu_* MCP tools, the bridge, doctor, config schema, and
    "what kimetsu is NOT." Consolidates the prior KIMETSU-CHAT,
    MEMORY-PROPOSALS, MEMORY-USEFULNESS, DEPENDENCIES into one
    self-contained reference.

WHAT v0.5.4 DELETES FROM THIS REPO
  * `docs/V0.3.4-SHIP.md`, `docs/V0.3.5-PERF.md`, `docs/V0.4-ROADMAP.md`,
    `docs/V0.5-PLAN.md`, `docs/SWEBENCH.md`: historical planning + ship docs.
  * `docs/KIMETSU-CHAT.md`, `docs/MEMORY-PROPOSALS.md`,
    `docs/MEMORY-USEFULNESS.md`, `docs/DEPENDENCIES.md`: content
    folded into HOW-KIMETSU-WORKS.md.
  * `docs/archive/` entire subtree (14 files: MP-4 through MP-15
    results, MVP, V0.2 plan/ship, V0.3 plan).

WHERE THEY WENT
  * All 22 files were copied to `docs/history/` in the private
    `github.com/RodCor/kimetsu-bench` repo (v0.5.4 commit on that
    side: "Adopt historical planning + ship docs from kimetsu repo").
    Their git history through v0.5.3 stays in this repo for bisects
    + archaeology.

README + CHANGELOG TOUCHED
  * README's "Documentation Map" section now points at one file:
    `docs/HOW-KIMETSU-WORKS.md` + the per-release CHANGELOG +
    per-crate `src/lib.rs` doc comments. Stale references to
    deleted docs removed.
  * CHANGELOG's v0.5.0 + v0.3 + v0.2 + v0.1 entries dropped their
    "see X.md" references; the X.md files are gone. The notes below
    each version still stand alone.

NET DELTA
  * kimetsu/docs/: 22 files → 1 file.
  * No code changes; no API change; no test count change.
  * cargo test --workspace      258 / 258 passing  (unchanged from v0.5.3)
  * cargo build --workspace     clean at 0.5.4

UPGRADE NOTES
  * If you've been bookmarking specific historical docs:
      - Need V0.5-PLAN.md, V0.4-ROADMAP.md, MP-* results? They're in
        the private bench repo. Ping for access.
      - Need KIMETSU-CHAT / MEMORY-PROPOSALS / MEMORY-USEFULNESS /
        DEPENDENCIES content? It's all in
        `docs/HOW-KIMETSU-WORKS.md` now (sections 1, 4, 4, 10).
  * Pre-v0.5.4 commit hashes still reference the files in their
    original locations; `git log` + `git show` work normally on
    history.

NEXT (in flight)
  * v0.5.5: Layer 2 driver implementation lands in kimetsu-bench
    (Python Harbor shim + BenchmarkDriver trait + Terminal-Bench
    impl + kbench CLI). The kimetsu_harbor/ directory in this repo
    gets deleted in the same release pass.

## v0.5.3: Layer 1 of the harbor refactor: in-process e2e suite + CLI smoke

First commit of the v0.5.3 harbor refactor arc. v0.5.0-v0.5.2 made
the brain learn from outcomes; v0.5.3 makes it possible to verify the
agent loop + brain pipeline still works *before pushing*, without API
keys or Docker. Catches the wiring-level regressions that per-crate
unit tests miss by construction.

WHAT v0.5.3 ADDS
  * New workspace crate `kimetsu-e2e` (publish = false, integration
    test harness only). Provides `ScriptedProvider` (pure-Rust builder
    over `MockProvider`), `TempProject` (per-test scratch project
    that wraps `project::init_project` + auto-cleanup), and brain-
    state assertion helpers.
  * Four scenario files in `crates/kimetsu-e2e/tests/`:
      golden_path.rs   one tool call + done (the smallest viable smoke)
      citations.rs     cite_memory → recorded_citations → memory_citations
      decay.rs         half-life ranking flip via the broker
      conflicts.rs     list_conflicts + resolve_conflict wrappers
    8 tests total. Runs in well under a second.
  * `crates/kimetsu-cli/tests/cli_smoke.rs`: 5 subprocess smoke
    tests that catch CLI argparse / subcommand / --help drift
    (no model calls, no network).
  * `KimetsuAgentOpts::for_tests()` is now `pub` (was `#[cfg(test)]`)
    so integration tests in `kimetsu-e2e` can use the same
    scripted-MockProvider-friendly settings as the harness's
    internal tests.

LAYER-2 (BENCHMARK) BOOTSTRAP
  * `/bench/` added to .gitignore. The internal benchmark
    orchestrator lives in a separate private repo
    (`github.com/RodCor/kimetsu-bench`) cloned into `./bench/` of
    the kimetsu working tree. Not in kimetsu's workspace; not in
    any release archive; invisible to `cargo install kimetsu-cli`.
    Subsequent v0.5.3.x commits land the driver impls there, the
    Python Harbor adapter migrates over, and `kimetsu_harbor/` gets
    deleted from this repo.

WHY TWO LAYERS
  * Layer 1 (this commit): in-process, deterministic, sub-second.
    Runs every push. Catches "did I break the wiring?"
  * Layer 2 (next commits, separate repo): real Claude Code / Codex
    against real Terminal-Bench tasks with kimetsu MCP attached vs
    detached. Comparative impact measurement. Runs on-demand.

TESTS
  cargo test --workspace      258 / 258 passing
    (was 239 at v0.5.2; +13 from the new e2e + cli_smoke layers)
  cargo build --workspace     clean at 0.5.3

UPGRADE NOTES
  * No user-visible API changes. Pre-push gate gets stronger.

NEXT (in flight)
  * v0.5.4: consolidate docs into a single HOW-KIMETSU-WORKS.md;
    historical planning + ship docs migrate to the internal
    kimetsu-bench repo.
  * v0.5.5: delete kimetsu_harbor/ from this repo (Python Harbor
    shim moved to kimetsu-bench in v0.5.4).

## v0.5.2: conflict detection at ingest: contradictions surface, don't silently compete

Third and final beat of the v0.5 arc. v0.5.0 attributed which memories
helped; v0.5.1 made stale boosters age out; v0.5.2 stops contradictory
memories from accumulating in the first place. "Use anyhow" and "use
thiserror" no longer both live in the brain quietly competing for
retrieval slots: the second write surfaces the conflict at ingest
time so the operator can decide which to keep.

WHAT v0.5.2 ADDS
  * New module `kimetsu_brain::conflict`. Top-level surface:
    `find_potential_conflicts(conn, scope, text, embedder, top_k,
    threshold)` returns `ConflictHit` rows whose cosine >= threshold
    AND whose normalized text differs from the incoming text.
    Defaults: `DEFAULT_CONFLICT_THRESHOLD = 0.8`, `DEFAULT_TOP_K = 3`.
  * Embedder gating. `embedder.is_noop()` short-circuits to zero
    hits, so lean builds keep exact pre-v0.5.2 behavior. Cross-model
    rows (embedding_model != active embedder id) are silently
    skipped: cosine across models is meaningless. A subsequent
    `kimetsu brain reindex` rehydrates them and the next ingest
    catches the conflict.
  * New schema: `memory_conflicts` table linking
    `(new_memory_id, existing_memory_id)` with `similarity`,
    `scope`, `kind`, `detected_at`, optional `resolved_at` +
    `resolution`. UNIQUE on the pair so re-scans stay idempotent.
    Created via `CREATE TABLE IF NOT EXISTS`; pre-v0.5.2 brain.db
    files pick it up on first open.
  * Wiring: both `project::add_memory` and `user_brain::add_user_memory`
    call `conflict::detect_and_record` after the post-insert
    embedding write. On a hit, one line to stderr; never blocks
    the write (surfacing > blocking; a blocked write loses user
    intent, a logged write loses nothing).

USER SURFACE: conflicts
  * `kimetsu brain memory conflicts [--limit N] [--json]` CLI:
    lists open conflicts merged from project + user brains,
    sorted by detected_at DESC. Each row shows similarity, scope,
    kind, and a one-line preview of both texts so the contradiction
    is visible at a glance.
  * `kimetsu brain memory conflicts --resolve <id> <kept_new|
    kept_existing|kept_both>`: settles a single conflict. With
    `kept_new` the existing memory is invalidated; with
    `kept_existing` the new memory is invalidated; with
    `kept_both` neither is touched (legit case where both apply
    in different contexts). Idempotent: a second resolve on the
    same id returns false without rewriting `invalidated_at`.
  * `kimetsu_brain_memory_conflicts` MCP tool (read-only): same
    backend, JSON-shaped for Claude Code / Codex. Resolution is
    deliberately CLI-only to keep the audit trail centralized.
    Brings the kimetsu_* MCP catalog to 18 tools.

NEW BRAIN API
  * `conflict::find_potential_conflicts(...)`: pure detection.
  * `conflict::record_conflict(...)`: idempotent insert keyed on
    the memory pair.
  * `conflict::detect_and_record(...)`: convenience wrapper used
    by the ingest path, returns the count of newly-recorded hits.
  * `conflict::list_unresolved_conflicts(conn, limit)`: joined
    with both memories' text for rich display.
  * `conflict::resolve_conflict(conn, conflict_id, resolution)`:
    settles a row, invalidates the losing side, returns true if
    something changed.
  * `project::list_conflicts(start, limit) -> Vec<ScopedConflict>`
    merges project + user brain rows with a `source` label.
  * `project::resolve_conflict(start, id, resolution)`: project
    DB first, user brain fallback. Acquires the project lock.

TESTS (12 new in brain: 10 conflict module + 1 project integration + 1 wrapper)
  conflict::tests (10)
    noop_embedder_returns_no_conflicts
    cross_model_rows_are_skipped
    exact_match_is_not_flagged_as_conflict
    similar_but_different_text_is_flagged
    record_conflict_is_idempotent
    list_unresolved_excludes_resolved_rows
    resolve_conflict_invalidates_loser_side
    resolve_conflict_is_idempotent
    detect_and_record_noop_writes_nothing
    resolve_conflict_rejects_invalid_resolution_strings
  project::tests (1)
    add_memory_distinct_texts_no_conflicts
      End-to-end regression: NoopEmbedder path produces zero
      conflicts, exercises list_conflicts + resolve_conflict
      wrappers (unknown id returns false, invalid resolution
      string errors out).

VERIFIED
  cargo test --workspace      239 / 239 passing
    (was 227 at v0.5.0, 239 now: +12 conflict tests)
  cargo build --workspace     clean at 0.5.2

UPGRADE NOTES
  * Existing brain.db files: `memory_conflicts` table created
    idempotently on first open with the v0.5.2 binary. No
    backfill; conflicts are only detected at fresh ingest.
  * Lean (default) builds: conflict detection is a silent no-op.
    Build with `--features embeddings` to enable. (Same gate as
    semantic retrieval.)
  * Threshold tuning: 0.8 cosine is BGE-small-en-v1.5's empirical
    "same concept" floor. If you see false positives in
    `kimetsu brain memory conflicts`, the surfaced pairs are
    similar-but-correct (e.g. two legit preferences for different
    contexts): resolve as `kept_both` to silence them. If you see
    false negatives (a real contradiction sneaking through),
    raise the threshold via a future config knob (deferred until
    real data justifies the surface).
  * The MCP tool is read-only by design. Operators resolve from
    the CLI; the host harness can list and reason about open
    conflicts but cannot apply resolutions. This keeps the audit
    trail centralized and prevents an agent from silently
    "resolving" a real contradiction it should have surfaced.

THE v0.5 ARC IS COMPLETE
  v0.5.0: citations: the brain knows *which* memories helped.
  v0.5.1: decay:     stale "useful" boosters age toward neutral.
  v0.5.2: conflicts: contradictory writes surface, don't compete.
  Together: the brain learns from outcomes, ages out stale signal,
  and stops accumulating noise. Pitch sharpens from "memory that
  follows you" to "memory that follows you AND improves on its own."

## v0.5.1: usefulness decay: recency-weighted memory ranking

Second beat of the v0.5 arc. v0.5.0 gave us *which* memories
helped; v0.5.1 makes "helped" age out. A memory that proved
useful 6 months ago shouldn't outrank one that proved useful
yesterday, yet under the v0.5.0 multiplier they tied, because
the boost was permanent. Long-running repos accumulated stale
boosters that crowded out fresh signal.

WHAT v0.5.1 ADDS
  * New column `memories.last_useful_at TEXT NULL`. Bumped by
    the projector ONLY on `(memory cited) AND (run.finished)`:
    cited + run.failed doesn't count (the memory misled the
    model), silent passengers never bump regardless of outcome.
    Distinct from `last_used_at` which still bumps on every
    retrieval. NULL on pre-v0.5.1 rows and on rows never
    cited successfully; the broker falls back to `created_at`
    for those.
  * New broker config `[broker.weights] decay_half_life_days`,
    default 30.0. `#[serde(default)]` so pre-v0.5.1 project.toml
    files keep loading cleanly. Set to 0 to disable decay.
  * New helper `kimetsu_brain::context::usefulness_decay(
    last_useful_at, created_at, half_life_days) -> f32` returning
    `exp(-ln(2) * age_days / half_life)` clamped to `[0, 1]`.
    Fail-open: unparseable timestamps and non-positive half-lives
    return 1.0 so retrieval never silently drops rows.

THE DECAY SHAPE
  decay attenuates the *deviation* from neutral, not the
  multiplier itself:
    effective = 1.0 + (raw_multiplier - 1.0) * decay
  At decay=1.0 a memory with the max +1.5 boost stays at +1.5.
  At decay=0.0 (very old) it slides back to 1.0 (neutral),
  same as a brand-new memory with zero history. Critically NOT
  zero: losing confidence in old signal shouldn't penalize a
  memory *below* a fresh one. Symmetric for the penalty side
  too: old penalties also fade toward neutral.

CALL CHAIN PLUMBING
  * `retrieve_context_with_embedder` reads
    `weights.decay_half_life_days` and threads it through
    `memory_candidates` → `{latest, fts}_memory_candidates` →
    `memory_row_to_candidate`.
  * Both retrieval SQL queries now also SELECT `last_useful_at`.

TESTS (7 new in brain, all in context.rs)
  context::tests::
    usefulness_decay_disabled_when_half_life_is_zero_or_negative
      Operator opt-out hatch: half_life <= 0 returns 1.0.
    usefulness_decay_returns_one_on_unparseable_timestamps
      Fail-open guard for corrupted rows.
    usefulness_decay_full_at_zero_age
      Future-timestamp (negative age clamped to 0) returns 1.0.
    usefulness_decay_follows_half_life_curve
      Asserts decay ≈ 0.5 at one half-life, ≈ 0.25 at two
      half-lives, computed against a real OffsetDateTime::now_utc.
    usefulness_decay_falls_back_to_created_at_when_last_useful_is_none
      Brand-new never-cited memories decay from their birthday,
      not from a hard 1.0 floor.
    aged_cited_memory_ranks_below_recently_cited_memory
      End-to-end: two FTS-tied memories, one cited yesterday and
      one cited a year ago: recent must rank first under the
      default 30-day half-life.
    aged_cited_memory_does_not_decay_when_half_life_is_zero
      Companion regression: with decay off, the same two
      memories tie on score. Proves the v0.5.1 flip is caused
      by decay, not by some unrelated timestamp side effect.

VERIFIED
  cargo test -p kimetsu-brain    86 / 86 passing  (was 79)
  cargo build --workspace        clean at 0.5.1

UPGRADE NOTES
  * Existing brain.db files: `last_useful_at` column added
    idempotently on first open with the v0.5.1 binary. All
    pre-v0.5.1 memories start at NULL → they decay from their
    `created_at` until the next successful citation refreshes
    them. No data loss; ranking will shift toward recently
    confirmed memories.
  * Existing project.toml files: no edit required.
    `decay_half_life_days = 30.0` applies automatically. To
    opt out, add `decay_half_life_days = 0.0` under
    `[broker.weights]`.
  * Tune the half-life per repo: lower (e.g. 14) for fast-
    moving codebases where knowledge ages quickly; higher
    (e.g. 90) for slow-evolving ones where old playbooks
    still apply.

NEXT (in flight)
  * v0.5.2: conflict detection at ingest. (Shipped above.)

## v0.5.0: the brain learns from outcomes: citations + blame

v0.5's north star: make the brain *get smarter over time* from
real run data. v0.5.0 ships the foundation (per-memory
attribution) that v0.5.1 (decay) and v0.5.2 (conflict detection)
build on. The arc is summarized in `docs/HOW-KIMETSU-WORKS.md`
sections 4-6; per-release detail is in the entries below.

PROBLEM
  Until v0.4.x the brain's usefulness signal was per-run, all-or-
  nothing: every memory in a run's `context.injected` event got
  +1 on `run.finished` or -1 on `run.failed`. A run that
  succeeded thanks to 1 of 10 retrieved memories rewarded all
  10 equally. Noise compounded over time: retrieved-and-ignored
  memories accumulated the same usefulness score as
  retrieved-and-pivotal ones.

WHAT v0.5.0 ADDS
  * New tool `cite_memory(memory_id, rationale?)`. The model
    calls it during a turn when it consciously used a
    retrieved capsule. Best-effort metadata: forgetting to cite
    doesn't fail the turn. Multiple citations per turn are fine.
  * New `memory.cited` event kind. The agent loop accumulates
    `cite_memory` calls into `recorded_citations` (annotated with
    the turn index), and the transport surface (chat REPL, harbor
    binary) emits one `memory.cited` event per citation to the
    trace at run wrap-up.
  * New schema: `memory_citations` table linking
    `(run_id, memory_id, turn)` with `cited_at` + optional
    `rationale`. Idempotent migration via `CREATE TABLE IF NOT
    EXISTS`; pre-v0.5.0 brain.db files pick up the table on first
    open with the new binary.
  * Projector handler `apply_memory_cited` mirrors each event
    into the new table.
  * Usefulness scoring split: cited memories get the strong
    ±1.0 delta, silent passengers (retrieved-but-not-cited)
    get the weak ±0.1 delta. Encourages models to actually use
    `cite_memory` and keeps the strong signal aimed at memories
    that actually contributed.

USER SURFACE: blame
  * `kimetsu brain memory blame <run-id> [--json]` CLI:
    prints cited memories with rationale + turn, then silent
    passengers with their text previews. JSON output for
    hooks/CI.
  * `kimetsu_brain_memory_blame` MCP tool: same backend
    (`project::blame_run`), JSON-shaped for Claude Code / Codex
    to consume. Listed in the 16+1 = 17 kimetsu_* tools advertised
    by `tools/list`.

NEW BRAIN API
  * `project::blame_run(start, run_id) -> BlameReport` walks
    `memory_citations` + `context.injected` events + the terminal
    run event, looks up each memory's text from project + user
    brains, returns `BlameReport { run_id, outcome,
    failure_category, cited, silent_passengers }`.

TESTS (3 new in brain, 1 net new since v0.4.11)
  brain::project::tests::
    run_finished_increments_usefulness_for_injected_memories
      Updated: now emits `memory.cited` so the test demonstrates
      the strong +1.0 signal path.
    run_failed_decrements_usefulness_unless_gate
      Updated: same, adds a citation so the failure penalty
      hits at strong -1.0.
    run_finished_gives_weak_signal_to_silent_passenger_memories  (NEW)
      Asserts: retrieved + uncited memory ends up at +0.1, not
      +1.0, on run.finished.
    blame_run_separates_cited_from_silent_passengers  (NEW)
      End-to-end: writes a run with 2 retrieved memories
      (1 cited, 1 silent), calls blame_run, asserts the cited
      one appears under `cited` with rationale + turn, the
      silent one under `silent_passengers`.

VERIFIED
  cargo test --workspace        227 / 227 passing
  cargo metadata --no-deps      clean at 0.5.0

UPGRADE NOTES
  * Pre-v0.5.0 chat / harbor runs continue to work; they just
    won't emit `memory.cited` events, so all their retrieved
    memories will be treated as silent passengers (±0.1 each).
    If you want the old "everything in context gets ±1" behavior
    back, the rule lives in
    `kimetsu_brain::projector::apply_memory_usefulness_for_run`.
  * Existing brain.db files don't need migration beyond opening
    them with the v0.5.0 binary: the `memory_citations` table
    is created on first open.
  * `kimetsu brain memory blame` on a pre-v0.5.0 run will
    typically show 0 cited + all retrieved as silent passengers
    (since no `memory.cited` events fired).

NEXT (shipped)
  * v0.5.1: usefulness decay. (Shipped above.)
  * v0.5.2: conflict detection at ingest. (Shipped above.)

## v0.4.11: drop x86_64-apple-darwin from the release matrix

The v0.4.10 release pipeline got stuck because two GitHub Actions
matrix jobs queued indefinitely:

```
build x86_64-apple-darwin (lean)        : queued, never started
build x86_64-apple-darwin (embeddings)  : queued, never started
```

As of late 2026, `macos-13` (Intel) runners are deprecated on the
GitHub Actions free tier and queue indefinitely without an SLA.
Apple Silicon (`macos-14` and newer, arm64) is the dominant
architecture and runs fine. Sitting in the queue for hours
blocked the `release` job → blocked the `publish-crates` job →
nothing actually shipped.

Fix in v0.4.11:

* `.github/workflows/release.yml` matrix drops the two
  `x86_64-apple-darwin` entries. The release matrix now ships
  6 archives (down from 8):
    * x86_64-unknown-linux-gnu   (lean + embeddings)
    * aarch64-apple-darwin       (lean + embeddings)  ← Apple Silicon
    * x86_64-pc-windows-msvc     (lean + embeddings)
* Users on Intel Macs can still `cargo install kimetsu-cli`
  (with or without `--features embeddings`): the source build
  is target-portable. They just don't get a pre-built binary.
* If GitHub re-provisions `macos-13` capacity in the future, we
  add it back; if x86_64 mac demand spikes, we can also cross-
  compile from `macos-14` (arm64 host): a v0.5 follow-up.

No code changes. v0.4.9's SecretString + v0.4.10's harbor-rs
publish exclusion both carry forward.

OPERATOR ACTION
  Cancel the stuck v0.4.10 workflow run on GitHub Actions
  (it'll never complete with those queued macOS jobs):
    gh run cancel <run-id>
    # or click "Cancel workflow" in the Actions tab UI

  Then v0.4.11's tag push fires a fresh, clean run.

## v0.4.10: kimetsu-harbor-rs stays out of crates.io

The v0.4.9 publish pipeline included `kimetsu-harbor-rs` in the
registry rollout. Reviewing pre-flight, that was wrong:

  * `kimetsu-cli` (the binary `cargo install kimetsu-cli` produces)
    does not depend on `kimetsu-harbor-rs`. End users never reach
    it through the registry path.
  * Harbor is a Terminal-Bench operator tool, still iterating
    internally. Publishing implies API stability we don't want
    to commit to yet.
  * The `kimetsu-harbor-agent` binary that benchmark operators
    actually use ships in every GH Release archive built by the
    matrix job (lean flavor). That stays.

Fixes in v0.4.10:

  * `crates/kimetsu-harbor-rs/Cargo.toml` adds `publish = false`.
    A manual `cargo publish -p kimetsu-harbor-rs` now refuses
    outright with a clear error.
  * `.github/workflows/release.yml` drops the
    `publish kimetsu-harbor-rs` step. The publish-crates job now
    walks 5 crates (core → brain → agent → chat → cli), not 6.
  * Summary block updated: "Published 5 crates" + an explicit
    note that harbor-rs is intentionally not published.

No code changes. No new tests. v0.4.9's SecretString + automated
publish work all carries forward.

## v0.4.9: SecretString for provider tokens + automated crates.io publish

SECURITY
  Both `ClaudeCodeProvider` and `AnthropicProvider` previously held
  `api_key: String` and derived `#[derive(Debug)]`. Any `{:?}`
  print of either struct (panic backtrace, `dbg!` left in a debug
  session, `tracing::debug!(?provider)` from a future telemetry
  pass) would have written the raw OAuth token / API key to
  stderr or the log sink.

  v0.4.9 introduces `kimetsu_core::secret::SecretString` whose
  `Debug` / `Display` / `serde::Serialize` impls all emit
  `"[REDACTED; len=N]"`. Cleartext is only reachable via
  `expose_secret()`: every caller is now greppable in code
  review.

  Provider fields converted:
    ClaudeCodeProvider.api_key: String -> SecretString
    AnthropicProvider.api_key: String -> SecretString

  Cleartext leak points (greppable, intentional):
    crates/kimetsu-agent/src/claude_code.rs
      .env("CLAUDE_CODE_OAUTH_TOKEN", self.api_key.expose_secret())  (2 sites)
      redact_token(..., self.api_key.expose_secret())                (3 sites)
    crates/kimetsu-agent/src/anthropic.rs
      .header("x-api-key", self.api_key.expose_secret())             (1 site)

  Regression guards:
    kimetsu_core::secret::tests
      debug_format_never_includes_inner_value
      display_emits_redaction_marker
      serialize_emits_redaction_marker
      expose_secret_returns_cleartext
      parent_struct_derive_debug_does_not_leak
    kimetsu_agent::claude_code::tests::debug_format_does_not_leak_api_key
    kimetsu_agent::anthropic::tests::debug_format_does_not_leak_api_key

  Pre-existing v0.4.5 `kimetsu_brain::redact` already covers the
  ingest-side leak surface (token strings in memory text); this
  patch closes the in-memory-struct surface.

DISTRIBUTION
  Per-crate Cargo.toml: every `path = "../kimetsu-X"` now also
  declares `version = "0.4.9"`. Required for `cargo publish`
  to resolve cross-crate deps via the registry instead of the
  local path.

  .github/workflows/release.yml gains a `publish-crates` job
  that runs after the binary matrix + GH Release succeed. The
  job uses `${{ secrets.CARGO_REGISTRY_TOKEN }}` and publishes
  the six crates in dependency order with a 30-second sleep
  between each so the crates.io index can propagate:
    kimetsu-core -> kimetsu-brain -> kimetsu-agent
                 -> kimetsu-harbor-rs -> kimetsu-chat
                 -> kimetsu-cli

  ONE-TIME SETUP (operator action):
    gh secret set CARGO_REGISTRY_TOKEN < <(cat path/to/token)
  Or via the GitHub UI: Settings -> Secrets -> Actions.
  The job hard-errors with an actionable message if the secret
  is missing, so a misconfigured pipeline fails fast.

  After this tag ships, end-users can:
    cargo install kimetsu-cli
    cargo install kimetsu-cli --features embeddings

  v0.4.7 + v0.4.8 GH Releases exist but were never published to
  crates.io (the workflow didn't have the publish job). v0.4.9 is
  the first crates.io-published version.

NEXT
  Smoke-validate with `kimetsu doctor` against a fresh
  `cargo install kimetsu-cli` to confirm the registry flow works
  end-to-end. If anything breaks per-platform, cut v0.4.9.1.

## v0.4.8: release-pipeline patch

The v0.4.7 release workflow failed across every platform with:

```
error: the package 'kimetsu-cli' does not contain this feature: embeddings
help: package with the missing feature: kimetsu-brain
```

Cargo doesn't auto-forward features across workspace dep chains:
the `embeddings` feature lived on `kimetsu-brain` but the release
matrix called `cargo build -p kimetsu-cli --features embeddings`,
which can't propagate down to a dep.

v0.4.8 adds a passthrough `embeddings` feature on every crate
that depends on `kimetsu-brain`: `kimetsu-cli`,
`kimetsu-chat`, `kimetsu-agent`, `kimetsu-harbor-rs`. Each one
declares:

```toml
[features]
default = []
embeddings = ["kimetsu-brain/embeddings"]
```

`kimetsu-cli` in particular fans out to all four downstreams so
`cargo install kimetsu-cli --features embeddings` builds the
whole tree on the embeddings code path.

No behavior change beyond unblocking the release pipeline. The
v0.4.7 tag stays in git history but its corresponding GitHub
Release was never published (the pipeline failed before upload).

## v0.4.7: distribution path

- **Per-crate `Cargo.toml` metadata** filled in for crates.io
  publish: `description`, `repository`, `homepage`,
  `documentation`, `readme`, `rust-version`, `keywords`,
  `categories`. Workspace `license` flipped from `UNLICENSED`
  (which crates.io rejects) to dual `MIT OR Apache-2.0`,
  matches the Rust ecosystem norm.
- **`LICENSE-MIT` + `LICENSE-APACHE`** files added at repo root.
- **`.github/workflows/release.yml`** ships tag-driven release
  pipeline: pushes a `v0.4.x` tag and CI builds release binaries
  for Linux/macOS/Windows × {lean, embeddings} flavors, runs
  `kimetsu doctor --skip-mcp` against each, attaches the archives
  to the GitHub Release, and pulls release notes from this
  CHANGELOG.
- **`kimetsu doctor` runs as a release gate** before any artifact
  uploads, so a broken build can never become a published release.

## v0.4.6: `kimetsu doctor` (automated wire-health)

- New `kimetsu doctor [--json] [--workspace PATH] [--skip-mcp]`
  CLI subcommand. Runs 8 hermetic checks (workspace, brain,
  safety, retrieval, mcp, plugin) and reports Pass / Warn /
  Fail / Skip per check with an actionable next-step on warns.
- Live-validated against this repo: 6 pass / 1 warn / 0 fail /
  1 skip, proving v0.4.1 (user brain), v0.4.4 (ambient), and
  v0.4.5 (redact) are all wired correctly end-to-end.
- `kimetsu-cli` test count: 2 → 6 (+4 doctor tests).

## v0.4.5: secret redaction at ingest

- New `kimetsu_brain::redact` module: `redact_secrets(text) ->
  RedactionResult` with non-overlapping greedy coverage across
  **13 secret kinds** (anthropic_oauth, openai_api_key,
  github_pat, slack_token, aws_access_key, jwt,
  private_key_pem, google_api_key, generic_bearer,
  generic_api_key, generic_token, generic_password).
- Wired at every memory write boundary: `project::add_memory`,
  `user_brain::add_user_memory`, `propose_benchmark_memory`.
  Redaction is idempotent; double-call is safe.
- On a hit, prints a one-liner to stderr (`kimetsu-brain:
  redacted 1 secret: anthropic_oauth`). Write proceeds with the
  redacted text; the surrounding context is preserved.
- 12 unit tests + 1 end-to-end test proving `sk-ant-...` never
  reaches brain.db.

## v0.4.4: ambient pre-turn context

- New `kimetsu_brain::ambient` module: collects git branch,
  `git status --short` top entries, top-5 mtime-ordered recent
  files (via the `ignore` crate, `.kimetsu/` filtered).
- `render_as_query_suffix(&ctx)` appends a short suffix like
  `\n[workspace: branch=X | recent: a.rs, b.rs | dirty: M ...]`
  to the explicit `query` before retrieval, so terse queries
  ("fix it", "continue") still surface useful capsules.
- Wired into `kimetsu brain context [--no-ambient]` CLI and into
  the MCP `kimetsu_brain_context` + `kimetsu_benchmark_context`
  tools (per-call `include_ambient` parameter, default true;
  global kill-switch `KIMETSU_BRAIN_AMBIENT=off`).

## v0.4.3: fastembed-rs backend + `kimetsu brain reindex`

- `fastembed = "5"` added as an OPTIONAL dependency behind the
  `embeddings` Cargo feature. Default build stays dep-light;
  opt in with `cargo install kimetsu-cli --features embeddings`.
- Three builtin models selectable via `KIMETSU_BRAIN_EMBEDDER`:
  `bge-small-en-v1.5` (default, 384 dim), `bge-m3` (1024 dim,
  multilingual), `jina-v2-base-code` (768 dim, code-tuned).
- `open_default_embedder()` returns a cached embedder via
  process-static `OnceLock`: model loads once per process.
- New `kimetsu brain reindex [--scope project|user|all]
  [--dry-run] [--force] [--limit N]` CLI subcommand backfills
  NULL embeddings AND rows whose `embedding_model` doesn't
  match the active embedder.

## v0.4.2: embeddings + hybrid retrieval scaffolding

- New `kimetsu_brain::embeddings` module: `Embedder` trait,
  `NoopEmbedder` (production default through v0.4.2),
  `StubEmbedder` (deterministic test pseudo-embedder), cosine +
  little-endian f32 BLOB codec helpers.
- `memories.embedding BLOB NULLABLE` + `embedding_model TEXT
  NULLABLE` schema columns. Migrated idempotently via
  `add_column_if_missing`.
- `retrieve_context_with_embedder` blends cosine with FTS as
  `final = (1 - α) * lex + α * normalized_cos` with `α = 0.5`.
  Cross-model rows skip the cosine term safely.

## v0.4.1: user-scope brain at `~/.kimetsu/brain.db`

- New `kimetsu_brain::user_brain` module. `MemoryScope::GlobalUser`
  writes now route to `~/.kimetsu/brain.db` (or
  `$KIMETSU_USER_BRAIN_DIR/brain.db` for tests / power users).
- `BrainSession` opens both DBs and merges retrievals across them
  via the new `retrieve_context_multi` path. Repo memories stay
  per-project; user memories follow the user between repos.
- Kill-switch: `KIMETSU_USER_BRAIN=0` falls back to v0.3.5
  behavior.

## v0.3: chat client + bridge plugin + prompt-cache visibility

The v0.3 line introduced the chat client, the bridge plugin
mode (MCP sidecar for Claude Code and Codex), and Anthropic
prompt-cache visibility + the persistent claude subprocess
that makes cache_read actually land. v0.3.5 flipped the
persistent path to default-on for chat.

## v0.2: Terminal-Bench validation

The v0.2 line ran the MP gauntlet from MP-4 through MP-18:
broker design + retrieval scoring, the 20-tool surface,
auto-orient pre-shell, parallel `tool_calls` envelope,
record_deviation + iterative verify.

## v0.1: initial scaffold

Brain + agent + pipeline foundations.

> Per-release planning + ship docs for v0.1 through v0.4 (the MP-*
> gauntlet result notes, the MVP doc, the V0.2 / V0.3 / V0.4 plans,
> the V0.5 plan) moved to the internal kimetsu-bench repo in v0.5.4.
> They remained in this repo's git history through v0.5.3; check
> commits prior to the v0.5.4 tag if you need them.
