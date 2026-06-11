# Kimetsu Roadmap

**North star:** growth through individual developers, whose sharpest pain is
**token spend**. Every release ladders up to one promise — *kimetsu pays for
itself* — and every claim ships with a measurement, the way v1.0 shipped
benchmarked retrieval ($0.19/win vs $2.47/win on the recorded Terminal-Bench
slice; recall@4 0.949 / MRR 0.914 on the 100-memory benchmark).

This document holds the release themes (the "big accomplishments") and the
v1.5 decomposition into epics → stories → tasks. Later releases stay at
big-rock level until their planning cycle starts.

| Release | Theme | One-line promise |
|---------|-------|------------------|
| v1.5 | **Pays for itself** | See what kimetsu saves you — and watch it tune itself to your brain. |
| v2.0 | **Never explore twice** | The biggest token sink is re-exploration; kill it. |
| v2.5 | **From recall to reasoning** | Break the retrieval ceiling; memories graduate into skills. |
| v3.0 | **Skip what others already paid for** | Knowledge packs, sync, and an SDK — memory as a shared substrate. |

---

## v1.5 — "Pays for itself"

Twin flagships: the **ROI ledger** (see the savings) and the **Self-Tuning
Brain** (watch retrieval improve with use). Supporting cast: token-budget
intelligence, memory consolidation, and reach.

### Epic 1: The ROI ledger *(flagship — see the savings)*

Make savings a number the user sees weekly, with honest methodology.

- **Story 1.1 — `kimetsu brain roi`**: tokens injected vs. estimated tokens
  saved (citation-weighted: a cited memory's saved-exploration estimate × hit
  count), converted to $ via the session model's pricing.
  - Task: savings-estimation model as a pure, unit-tested function —
    conservative by design (under-claim, never over-claim).
  - Task: accounting over existing `context.served` + citation events (no new
    collection).
  - Task: CLI table + `--json`.
- **Story 1.2 — Stop-hook savings line**: one line at session end — *"Kimetsu:
  ~3.2k tokens saved this session (2 memories cited)."* The visibility loop.
  - Task: extend the stop-hook JSON (`systemMessage`).
  - Task: per-session ledger query.
- **Story 1.3 — Honest-methodology doc**: publish exactly how savings are
  estimated, calibrated against the Terminal-Bench evidence. (Also the answer
  to the inevitable HN methodology thread.)

### Epic 2: Token-budget intelligence *(every token earns its place)*

- **Story 2.1 — Injection-time capsule compression**: long memories are
  distilled to their actionable core at injection; full text stays one
  expansion-handle away.
  - Task: rule-based pass first (strip `[tags: …]` / `(context: …)` at
    injection, sentence cap).
  - Task: measure tokens-per-capsule before/after in the bench.
- **Story 2.2 — Confidence-gated injection**: skip more aggressively when top
  relevance is marginal — silence is cheaper than noise.
  - Task: tune `min_score` against the eval harness with a token-cost-weighted
    objective.
- **Story 2.3 — Cross-turn dedupe hardening**: audit the per-run ledger for
  gaps where the same capsule re-injects across hooks/turns; add a bench
  scenario.

### Epic 3: Memory consolidation *(denser brain = cheaper injections)*

- **Story 3.1 — Near-duplicate merge**: embedding-similarity clusters above a
  threshold merge into one memory (provenance and usefulness preserved).
  - Task: cluster detection over existing vectors.
  - Task: merge operation (citations + usefulness sum; originals retired, not
    deleted).
  - Task: `kimetsu brain consolidate --dry-run`.
- **Story 3.2 — Cluster distillation**: N related episodic lessons → one
  distilled principle, via the existing cheap-model distiller infrastructure;
  lands as a proposal for review.
  - Task: cluster-selection heuristics.
  - Task: distiller prompt + accept/review flow reusing `memory_proposals`.
- **Story 3.3 — Fading-memory triage**: surface the fading set (usefulness
  < 0.2) with a one-command keep / merge / prune decision flow.

### Epic 4: The Self-Tuning Brain *(flagship — watch it get better)*

**The unfair advantage:** kimetsu already collects ground truth nobody else
has. Every `context.served` event records what was retrieved; every citation
records what actually *helped*. That is a free, ever-growing, personal eval
set — the brain can optimize retrieval against the user's REAL queries, not a
synthetic fixture.

- **Story 4.1 — Personal eval set from telemetry** *(the foundation)*:
  (query → cited memory) pairs become positive cases; injected-but-never-cited
  becomes the noise signal.
  - Task: eval-set builder over the events tables (query_hash → raw-query
    retention policy needed; store queries with consent flag).
  - Task: `kimetsu brain tune --status` — how many ground-truth cases the
    brain has accumulated.
- **Story 4.2 — Guarded one-shot tune**: `kimetsu brain tune` sweeps floors /
  pool / reranker against the personal eval set (synthetic fixture fallback
  below a case-count threshold), with a token-cost-weighted objective
  (quality per injected token).
  - Task: tune loop over the existing bench/eval machinery.
  - Task: safety rails — must beat the current config on held-out cases
    before applying; bounded search space.
  - Task: config writes carry provenance comments; `tune --revert` restores.
- **Story 4.5 — Regret tracking**: log skipped-but-would-have-been-cited
  events — the error signal that closes the floors' feedback loop.
  - Task: regret event on citation of a capsule that the floors had dropped
    in an earlier turn of the same run.

*(Stories 4.3 — continuous re-tune triggers — and 4.4 — model re-selection
advisor — ship in v2.0; see below.)*

### Epic 5: Reach *(more developers, same brain)*

- **Story 5.1 — `kimetsu brain export --redact`** (issue #24; also unblocks
  v3.0 knowledge packs).
- **Story 5.2 — Cursor installer**; **Story 5.3 — Gemini CLI installer**.
  Verify each host's real config surface against its actual repo before
  writing embedded assets — inferred APIs have been wrong before.
- **Story 5.4 — CI embeddings test job** (issue #22 — protects everything
  above).
- **Story 5.5 — Remote bench process isolation** (issue #23) — opportunistic.

**Sequencing instinct:** Epics 1 + 4 are the release's identity and land
first; 3 is the deepest engineering; 2 is incremental and safe; 5 is
parallelizable filler.

---

## v2.0 — "Never explore twice" *(big rocks)*

1. **Session warm-start packs** — inject a distilled project digest at
   session start (what the repo is, conventions, current focus) so the agent
   never re-derives the basics. The single biggest re-exploration sink.
2. **Answer-from-memory short-circuits** — when a memory directly answers
   (command recall, config locations), serve it without an LLM round-trip.
3. **Continuous self-tuning** *(Stories 4.3 + 4.4)* — re-tune proposals on
   corpus milestones / drift detection; model re-selection advisor (measured:
   embedder rankings flip with corpus size — bge won at 18 memories, jina-v2
   at 100). Proposed, never silently applied; reindex cost stated.
4. **Cheap/local-model offload** — harvest, distillation, consolidation, and
   LLM-assisted steps routable to local endpoints (Ollama via the existing
   OpenAI-compatible base-URL support) — token savings for the
   most-cost-sensitive users.
5. **Personal multi-machine sync** — your brain on every machine without
   running a server (Remote covers the server path; this is the local-first
   path).

## v2.5 — "From recall to reasoning" *(big rocks)*

1. **Memory graph** — typed links (causes / supersedes / refines); retrieval
   walks one hop. Attacks the measured ~0.93 ceiling: oblique multi-hop
   queries ("CI froze forever" → the mutex-deadlock lesson) defeat every
   embedder×reranker combo — that class needs structure, not bigger models.
2. **LLM-assisted retrieval** — cheap-model query expansion / HyDE for
   conceptual queries, on the distiller's credentialed-cheap-model pattern.
3. **Memory → skill synthesis** — a lesson cited N times auto-proposes as a
   bridge skill/command. Memories graduate into capabilities; a skill is the
   ultimate token saver.
4. **Benchmark v2** — multi-hop and adversarial cases; a public results page
   (marketing surface as much as engineering tool).

## v3.0 — "Skip what others already paid for" *(big rocks)*

1. **Knowledge packs** — publish/install curated brains
   (`kimetsu brain install rust-gotchas`); redaction (v1.5) + consolidation
   (v1.5) make packs safe and dense.
2. **Pack registry** — discovery, versioning, provenance.
3. **Plugin SDK** — stable extension points for custom hosts, embedders, and
   storage backends.
4. **Teams/Remote GA track** — promoted from backlog only if team demand
   materializes; the individual-developer path stays primary.

---

## Operating principles (carried from v1.0)

- **Every claim ships with a measurement** — features that affect retrieval
  run through `kimetsu brain bench`/`eval` before defaults change.
- **Conservative estimates** — the ROI ledger under-claims by design.
- **Nothing silently changes behavior** — tuning proposes; the user applies.
- **Local-first, single-file, no telemetry** — non-negotiable identity.
