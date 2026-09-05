# Sync integration follow-up

Scope: final-integration-review.md findings 6 and 7. Owned source is sync.rs plus the visibility of projector::replay_locked; no unrelated implementation edits, live brain/config access, model loads, downloads, or subagents.

The importer previously projected individual historical events against the current projection before directory sync reached its final replay. A correction predating an already applied retirement therefore failed. Individual commits also retained earlier imported events if a subsequent JSON line or projection failed, and pull cursors were persisted before the final rebuild. memory.restored was missing from the replication allowlist.

The importer now parses the batch first, stages unseen redacted events under BEGIN IMMEDIATE, and replays the merged durable log before committing. Duplicate counts are calculated under that same writer lock; dry-run uses an in-batch ID set so its counts match committed imports. Directory sync reads all pending peer batches before one import, permitting dependencies across peer directories and avoiding partial peer imports. Pull cursors are persisted only after a successful database commit. The existing replay helper preserves the missing-unlogged-memory guard and causal context.injected revision binding. Restoration now exports and imports as a durable lifecycle event.

Seven new behavior regressions cover correction-before-retirement (both explicit invalidation and archival), archive/restore replication, whole-batch rollback on malformed JSON or invalid projections, prerequisites in a later peer directory, cross-peer rollback with unchanged pull cursors, legacy unlogged-memory protection, and consistent duplicate counts. The correction test also checks the earlier exposure's baseline revision binding, FTS exclusion, replay stability, and cursor/idempotency behavior.

Verification on 2026-09-04:

- RED: `cargo test --offline --locked -p kimetsu-brain --lib sync::tests` — 8 existing passed, all 7 new tests failed for their intended defect. Log: tmp-tests/sync-followup-red.log. This first run used the default worktree target before root clarified the shared target; no dependencies were downloaded.
- GREEN with `CARGO_TARGET_DIR=E:/Kimetsu/target`: same command — 15 passed, 0 failed. Log: tmp-tests/sync-followup-green.log.
- Covering with the same target: `cargo test --offline --locked -p kimetsu-brain --lib projector::` — 37 passed, 0 failed, including trace-import revision binding, missing-memory guard, correction rollback, and writer-lock snapshot regressions. Log: tmp-tests/sync-followup-projector.log.
- `rustfmt --edition 2024 crates/kimetsu-brain/src/sync.rs` and scoped `git diff --check` completed successfully. Git only reports the repository's LF-to-CRLF conversion warning.

Limits: whole-log replay and pending batches require memory proportional to their size, consistent with the existing maintenance replay approach. Cursor files and SQLite cannot commit atomically; a cursor-file write failure after the database commit is recoverable by idempotent re-import. The independent push phase may already have published a local batch when a pull fails. Existing legacy HLC synthesis/order limitations and incomplete durable-log migration remain unchanged. Root owns independent integration review, the full suite, feature-enabled tests, and empirical model/latency validation.
