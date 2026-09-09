# Kimetsu agent-memory audit — 2026-09-04

Audited `E:/Kimetsu`, commit `3ec56a8`, version 2.7.0. Scope: local brain, retrieval, MCP/CLI/hook delivery, feedback, temporal correctness, persistence, benchmark methodology, and selected remote/ANN paths. Sibling repositories were not audited. This is an audit and proposed roadmap; production code was not changed.

**Verdict:** Kimetsu has a strong foundation, but it is not yet a reliably bounded, self-correcting memory for an agent. The biggest gaps are at the interfaces between existing features: rejected memories still reach the model, edits do not survive replay, time validity is incomplete, and citations are treated as successful outcomes. Fix those before adding more retrieval models or graph machinery.

## What “0 tokens” can actually mean

| Target | Feasibility |
|---|---|
| Zero paid model calls for storage, indexing, retrieval, and maintenance | Achievable with local deterministic processing and local embeddings/reranking. |
| Zero generative LLM tokens anywhere in memory processing | Achievable only when distillation, local `ask`, HyDE, and host-agent harvesting are disabled or replaced with deterministic capture. Local generation still uses tokens, even without an API bill. |
| Zero memory tokens added on irrelevant turns | Achievable through silent host-side abstention, before invoking the model. |
| Arbitrary new memories influence a hosted model while consuming no input tokens | Not achievable through ordinary MCP or text injection. The model must receive the information. |
| Lower total agent tokens with memory enabled | Achievable in principle; requires paired measurements of task success and total consumption. |

Prompt caching reuses prompt computation; it is not token-free semantic memory. OpenAI documents cached-input accounting and cost measurement in its [prompt caching guide](https://developers.openai.com/api/docs/guides/prompt-caching).

The useful product promise is: **“No paid inference for memory operations; silent when unnecessary; bounded context when useful; measured reduction in total agent cost.”** Do not call embedding and cross-encoder inference “model-free”; “no generative LLM calls” is accurate.

## Existing strengths worth preserving

- Rust, SQLite/FTS5, optional local embeddings, HNSW, and a warm daemon are a sensible local architecture.
- The system already has provenance weights, remote-pack quarantine, redaction, supersession, decay, negative feedback, retrieval abstention, chronological rendering, episodes, and injection-policy training. These are not missing features.
- Expandable capsules, per-session suppression, and compression provide the basis for compact context delivery.
- Replay, migrations, host smoke tests, and synthetic memory evaluations give substantial regression coverage.
- The README discloses several benchmark comparability limits. Keep that transparency and make consumption equally visible.

## Prioritized findings

### 1. P0 — MCP bypasses its own context budget

**Confirmed in current source and reproduced against the freshly built lean binary.**

`kimetsu_brain_context` serializes both selected `capsules` and complete `excluded` capsules. Excluded entries include their summaries. The agent therefore receives memories rejected by the budget/ranking pipeline. Diagnostics are mixed into the model-facing response, along with verbose guidance, metadata, and repeated query information.

An isolated 26-memory fixture requested 100 tokens, which the endpoint clamped to 500. It reported **186 used tokens, three selected capsules, and 23 excluded capsules**, but emitted **37,868 characters on the JSON-RPC wire**, including **11,860 characters of excluded summaries alone**. Wire characters are not tokenizer counts, and some bytes are transport encoding; the excluded text is nevertheless demonstrably in the tool payload. The preexisting release binary reproduced the same result.

**Change:** Default to a compact response containing selected evidence, short stable handles, necessary validity/provenance, and expansion access. Return excluded counts only. Move scoring details and excluded text into a separate opt-in inspection surface. Enforce the budget on the final model-visible representation, including framing and warm-start content.

**Acceptance:** Growing the rejected candidate pool must not grow the normal tool response. A tokenizer-based contract test must cover the entire returned content.

Evidence: [mcp_server.rs](../../crates/kimetsu-chat/src/mcp_server.rs#L858), especially line 896; the benchmark-context response also serializes excluded entries near line 1168.

### 2. P0 — Corrected memories revert after rebuilding

**Confirmed and reproduced.** `edit_memory` directly updates SQLite text, kind, FTS, and embeddings without writing a corresponding durable correction event. Replaying the original event log restores the original memory.

Probe: add “The audit service uses sqlite checkpoint before deployment migration.” → edit to “Corrected audit lesson: use verified recovery procedure.” → `brain rebuild` → original text returns.

This breaks the central promise that the brain learns corrections. The edit also preserves confidence/usefulness even when the proposition changes, and the sequence of database updates is not one atomic correction transaction.

**Change:** Make corrections immutable, revisioned events; project text, FTS, validity, and embedding invalidation atomically. Preserve lineage while distinguishing evidence for the old claim from evidence for the new claim. Ensure sync exports/imports correction events.

**Acceptance:** Edit → restart → rebuild → sync round trip preserves the correction. Interruption cannot leave text and FTS disagreeing. Historical queries return the appropriate revision.

Evidence: [project.rs](../../crates/kimetsu-brain/src/project.rs#L2026); existing edit tests near line 5572 check immediate results, not replay.

### 3. P1 — Hidden full scans undermine retrieval scaling

**Confirmed SQL shape; measured isolated SQL workload.** Both IDF functions issue `COUNT(*) ... lower(text) LIKE '%term%'` once per query term. The query plan is `SCAN memories`. A normal retrieval can invoke both lexical-floor and evidence-coverage computations. HNSW does not remove these costs.

Eight query terms, synthetic roughly 200-character rows, Python SQLite in memory, five samples per scale:

| Memories | Median time for one eight-term IDF pass |
|---:|---:|
| 1,000 | 4.03 ms |
| 10,000 | 41.26 ms |
| 100,000 | 437.72 ms |

These are not full Rust broker timings, semantic-model timings, or million-memory results. They demonstrate linear work in a helper on the retrieval path. Another growth risk: query routing loads every qualifying route and computes similarity in Rust; it is not a bounded nearest-neighbor lookup despite the “one indexed SQL read” description.

**Change:** Maintain token document frequencies incrementally, or use an appropriate FTS vocabulary with consistent token semantics; share/cache them by corpus revision. Bound and index route retrieval. Avoid replacing substring semantics with FTS semantics without evaluation.

**Acceptance:** Profile the complete request, including SQL, candidate hydration, routing, reranking, serialization, and hooks. Report cold/warm p50/p95/p99, resident memory, and contention at 1K/10K/100K/1M memories.

Evidence: [context.rs](../../crates/kimetsu-brain/src/context.rs#L444), line 2414; [reinforce.rs](../../crates/kimetsu-brain/src/reinforce.rs#L315).

### 4. P1 — Current-time retrieval can return facts that are not currently valid

**Future-date failure reproduced; timestamp comparison reproduced independently.** Candidate SQL filters `valid_to` but omits `valid_from`. A memory with `valid_from=2099-01-01T00:00:00Z` was returned among selected capsules today.

Expiration also compares RFC3339 strings against SQLite `datetime('now')`, whose separator is a space. Lexicographically, `2026-09-04T01:00:00Z > 2026-09-04 12:00:00` is true although the fact expired eleven hours earlier. Offset variations add further problems unless timestamps are normalized.

The “bitemporal” view also uses a single timestamp for both axes and does not select historical text revisions. Direct edits are consequently incompatible with faithful historical beliefs.

**Change:** Use normalized numeric times and a shared predicate: `valid_from <= now < valid_to`, with explicit null semantics. Separate “valid at” from “known at” and retain revision history. Apply the same rules to FTS, ANN hydration, graph expansion, and historical views.

**Acceptance:** Cover future facts, same-day expiration, timezone offsets, late-arriving corrections, supersession, and replay with a controllable clock.

Evidence: [context.rs](../../crates/kimetsu-brain/src/context.rs#L1242), lines 1416 and 1502; [bitemporal.rs](../../crates/kimetsu-brain/src/bitemporal.rs#L59).

### 5. P1 — Free-tier gating does not prevent host-agent token spending

**Confirmed control-flow gap; no paid run was triggered during this audit.** `resolve_pipeline_distiller` returns `None` on Free. The stop hook interprets absence of a distiller as a reason to issue a blocking harvest cue when `auto_harvest` is enabled; that setting defaults to true. The cue requests a memory-harvester agent, re-entering host inference. Proactive hooks can also issue harvest cues.

**Change:** Make the zero-generative-token policy apply across host callbacks, harvesting, HyDE, reflection, and background jobs. In strict mode, collect structured tool outcomes deterministically and enqueue evidence locally. Distinguish strict/no-generation, local-generation, and paid-generation capabilities explicitly.

**Acceptance:** Replay a substantial session under strict mode and assert no generation provider, harvest agent, or stop continuation was invoked. No relevant memory means no injected text.

Evidence: [distiller.rs](../../crates/kimetsu-cli/src/distiller.rs#L622); [hooks.rs](../../crates/kimetsu-cli/src/commands/hooks.rs#L503), lines 533, 642, 931; [config.rs](../../crates/kimetsu-core/src/config.rs#L476).

### 6. P1 — Citations conflate attention, usefulness, and verification

**Confirmed control flow.** An MCP citation becomes a standalone citation with a fresh group ID and no query. The projector immediately applies a positive outcome and stamps useful activity. Trust treats `last_useful_at` as corroboration, removing origin penalties. This requires no independent successful test or final task outcome.

The benchmark helper further credits memories retrieved *after* a task passes, rather than requiring the exact IDs actually supplied to the agent. That can reward an unexposed memory. Repeated singleton MCP citations also fail to provide the grouped/query-linked signal needed by co-citation reinforcement.

**Change:** Record exposure, cited reliance, verified outcome, contradiction, and causal benefit separately. Bind feedback to session, task, query, memory revision, and evidence. Credit actual delivered IDs. One citation should not erase an origin penalty. Preserve negative evidence without treating every unused memory as harmful.

**Acceptance:** Citations without verification do not become “verified here”; an unexposed memory cannot receive task credit; failed tasks cannot receive successful-outcome reinforcement.

Evidence: [feedback.rs](../../crates/kimetsu-brain/src/feedback.rs#L175); [projector.rs](../../crates/kimetsu-brain/src/projector.rs#L365); [trust.rs](../../crates/kimetsu-brain/src/trust.rs#L148); [reinforce.rs](../../crates/kimetsu-brain/src/reinforce.rs#L73).

### 7. P1 — Token estimates and savings estimates are not enforceable measurements

`estimate_tokens` is `ceil(whitespace_words * 1.33)`. A long identifier, minified JSON, or unspaced CJK passage can count as roughly two tokens. Budgeting happens before some rendering changes, and MCP output contains additional material not counted by `used_tokens`.

The ROI ledger assigns fixed savings per citation/digest/resume and estimates output tokens using a 0.25 ratio. Those are assumptions about avoided work, not measured causal lower bounds. A cited memory can save nothing or cause extra work. Calling a constant conservative does not prove that it underestimates savings.

**Change:** Use the host tokenizer where available and a conservative byte-based bound otherwise. Measure final output. Track real input/output/cached usage when exposed; label missing usage unknown. Keep counterfactual savings estimates separate from observed consumption.

Evidence: [context.rs](../../crates/kimetsu-brain/src/context.rs#L2853); [roi.rs](../../crates/kimetsu-brain/src/roi.rs#L31), constants starting at line 58.

### 8. P1/P2 — Cross-process freshness needs a revision protocol

**Source-backed risk, not an embeddings-runtime reproduction.** ANN freshness checks compare maximum row ID. Re-embedding an existing memory in another process does not increase that watermark. The local process updates a cached ANN handle if it has one; that does not notify an already warm daemon elsewhere. Reconciliation adds rows above the watermark and removes retired rows, but does not refresh changed vectors at existing row IDs.

**Change:** Publish a monotonic corpus/change sequence for insert, edit, re-embed, invalidate, supersede, and rebuild. Let all indexes consume deltas, and bind cached results to the revision. Include embedding-model identity.

**Acceptance:** Keep reader/daemon A warm while writer B edits or re-embeds a memory. A must retrieve the new meaning without restart; old vectors must not occupy the useful top-K pool indefinitely.

Evidence: [ann.rs](../../crates/kimetsu-brain/src/ann.rs#L327), line 609; [embeddings.rs](../../crates/kimetsu-brain/src/embeddings.rs#L1004).

### 9. P2 — Agent continuity is scoped too coarsely

Episodes hold useful task/open-thread/dead-end fields, but there is one live episode per repo root. Independent tasks in the same checkout can replace one another's resume state. The payload does not include a task/session/branch/commit identity for that state.

For an agent like this one, reliable memory should distinguish: user preference, durable project rule, verified code fact, and current task state. Each needs different expiration and authority. Code claims need paths/symbols plus a revision or content fingerprint; task state needs branch/worktree/session keys and source evidence. Existing provenance and ambient context are a starting point, not substitutes for applicability checks.

Evidence: [episode.rs](../../crates/kimetsu-brain/src/episode.rs#L39), projection near line 117.

## Codex integration observations

This session started at `E:/`, outside the Kimetsu project directory, and exposed no Kimetsu MCP tools in the available tool catalog. Therefore Kimetsu was not automatically accessible to me through MCP in this session. That does not establish that the installed hooks are broken; their execution was not observed.

The checked-in hooks use `--workspace .`, so correct workspace resolution and actual host activation need an end-to-end probe. The checked-in prompt hook also lacks `--warm-on-first-prompt`, which the current installer generates. Presence of configuration files is insufficient evidence that the running host loaded them.

The `Bash` matcher itself is **not a bug**: current [official Codex hook documentation](https://learn.chatgpt.com/docs/hooks) explicitly maps `exec_command` to `Bash`. Add a doctor check that proves a hook ran and that a seeded nonce reached model-visible context, with the resolved repository and selected retrieval path recorded. Avoid automatic setup changes during a read-only audit.

## What would justify “best memory”

Existing QA scores do not establish the best agent memory under a small token budget. LongMemEval's harness configures a 48,000-token retrieval budget, yielding roughly 24,000 capsule-budget tokens, and uses a generative reader/judge. That is a different operating point from a 100–500-token coding-agent hint. The stored BrainBench report's 91.3% aggregate includes 122 calibration scenarios but only five retrieval scenarios, which score 56.6%; it is a historical artifact, not a rerun of this commit.

Build one held-out evaluation matrix:

- Same agent/model/tools/task budget, paired no-memory and memory runs; multiple seeds and confidence intervals.
- Cold, repeated, and related-but-new tasks; split by repository/task family/time to prevent training leakage.
- Wrong memories, changed branches, same-day expiry, corrections, malicious imported instructions, empty memory, and multilingual/code-heavy text.
- Retrieval at final-context budgets 0/128/256/512/1024 tokens, plus a larger diagnostic ceiling.
- Primary metrics: verified task success, total billed tokens/cost, time to completion, repeated-error rate, harmful/irrelevant injection rate, and recovery after interruption. Citations remain a secondary signal.

Evidence: [longmemeval.rs](../../bench/src/drivers/longmemeval.rs#L737); [stored BrainBench report](../../bench/brainbench-full.md).

## Recommended implementation order

1. **Restore trust in the contract:** compact MCP delivery, final-output budgeting, event-sourced corrections, correct temporal predicates, strict generation gating.
2. **Make runtime work bounded:** indexed/cached IDF, bounded route search, revision-driven ANN/cache refresh, stage latency metrics and deadlines.
3. **Improve what gets remembered:** passive structured capture of command failures/fixes and verified outcomes; distinct task/user/project memory; revision-aware applicability checks; explicit evidence grades.
4. **Prove benefit:** paired task evaluations and real usage accounting, with a held-out small-context track. Tune retrieval only against those results.

Suggested initial engineering targets—not measured achievements—are warm local retrieval p95 below 100 ms at 10K memories and below 250 ms at 100K; zero extra model context on irrelevant turns; 128–512 tokens for routine useful hints; and no task-success regression against the paired baseline. Measure larger corpora separately before promising million-memory interactive performance. Specify hardware and resident-memory budgets for each supported mode.

The existing architecture can support this direction. Replacing SQLite or introducing another generative layer is not currently justified by the evidence.

## Verification and limits

- `cargo test -p kimetsu-brain -p kimetsu-e2e --locked --offline`: 646 passed, three ignored, zero failures including doctests.
- `cargo test -p kimetsu-chat -p kimetsu-cli --locked --offline`: 414 passed, one ignored, zero failures.
- Total: **1,060 passed; four ignored; zero failures**. These are lean/default-feature runs, not the embeddings/remote/full-workspace matrix.
- [Reproduction script](../../tmp-tests/audit_probe_20260904.py) uses isolated temporary projects with user brain disabled. Final run uses the current-source debug binary produced by the host test build.
- [Probe output](../../tmp-tests/audit-probes-20260904.json) records MCP payload growth, edit/rebuild regression, future-validity leakage, timestamp comparison, and isolated SQL scaling. It includes synthetic payload text only.
- Test logs: [brain/e2e](../../tmp-tests/audit-tests-20260904.log), [chat/CLI](../../tmp-tests/audit-host-tests-20260904.log).
- No paid model calls, full public QA benchmark reruns, million-memory end-to-end measurements, live Codex-hook activation test, or cross-process embeddings test were performed. No global host configuration or existing brain was deliberately modified.
