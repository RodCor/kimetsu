# Structured Fact Evidence Implementation Plan

> Use superpowers:subagent-driven-development task-by-task, with independent files and serialized Cargo verification.

**Goal:** Store local fact evidence and deliver subject-bound partial-answer metadata.
**Architecture:** Redacted text -> SQLite fact projection -> capsule evidence -> final-slice answerability.
**Tech Stack:** Rust, rusqlite, regex, serde; no new models/dependencies.
**Spec:** docs/superpowers/specs/2026-09-07-structured-facts-design.md

## Global constraints
Keep guard opt-in. Preserve lifecycle, claim revision and byte budgets. No live configuration changes. Cargo --locked --offline -j1, one build at a time. Do not benchmark during builds/tests.

### 1. Conservative extraction
Files: create crates/kimetsu-brain/src/facts.rs; expose module in lib.rs.
Interface: `pub fn extract(text: &str) -> Vec<FactClaim>` with the five fields in the spec.
- [x] RED tests: `extract("Orchid staging gateway port is 7319.")[0]` has subject orchid gateway, environment staging, attribute port, value7319; unrelated database clause cannot acquire gateway value; negated/example/redacted claims emit no facts.
- [x] Implement explicit patterns for existing configuration attributes and literal keys, conservative clause parsing and bounded output.
- [x] GREEN extraction tests and review.

### 2. Durable projection
Files: fact_store.rs, schema.rs, migrate.rs, projector.rs, context.rs, core/lib.rs; update capsule initializers wherever needed.
Interfaces: `refresh(conn, memory_id)`, `backfill(conn)`, `load(conn, memory_id, claim_revision) -> KimetsuResult<Vec<StoredFact>>`.
- [x] RED tests migration/backfill, acceptance/correction, rebuild equality, invalidation and temporal refresh, revision mismatch.
- [x] Create schema15 projection indexed bymemory/revision; refresh event writes after redaction; hydrate alongside capsule claim identity.
- [x] GREEN storage and hydration tests; review.

### 3. Matching and final delivery
Files: fact_query.rs, answerability.rs, serving.rs, hooks.rs.
Interfaces: `parse`, `evaluate` and filtering/notice helpers consuming stored facts and finalcapsules.
- [x] RED tests wrong subject/environment, compound port+password partial, conflicting values, final-budget removal changes supported attributes, compression preservesvalue.
- [x] Implement conservative question decomposition and evidence reconciliation; integrate under guard; recompute metadata inside fit_json; hook notices from visible evidence.
- [x] GREEN focusedtests and review.

### 4. Evaluation and finish
- [x] Freeze new synthetic scope/partial fixture before inference; extend benchmark observations for answerability metadata if necessary.
- [x] Full workspace and benchmark tests; build release; actual hook/MCP probes.
- [x] Paired measured comparison preserving baseline executable; review findings; document limits and performance; commit verified work/artifacts.

## Ledger
Ruling: proceed within user's explicit go-ahead, without repeated approval gates. Prior answerabilityguard remains off by default. Storage/extractor independent tasks use subagents; main owns query/render integration. No concurrent Cargo builds.

Final: source3ae8329, benchmark2c74dad. Workspace1470 pass/6ignored; benchmark132Rust+18Python;6releaseprobespass. Campaign688observations, nohitlosses, newnoise15/18->3/18, exactmetadata36/45bothrepeats. Remainingcompoundretrieval and unsupportedsubjectfallback documented; noheldoutretuning, defaultoff.
