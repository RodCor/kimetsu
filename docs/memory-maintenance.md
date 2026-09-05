# Memory maintenance and replay

`brain compact --trim-events-older-than <duration>` now removes only expendable telemetry (`context.served`, `retrieval.stats`, `digest_served`, `resume_served`). Claim, correction, exposure, citation, outcome and unknown event kinds are retained. This deliberately reclaims less history than earlier versions: deleting accepted events left memories visible until a rebuild erased them.

Projection rebuild takes the SQLite writer lock before reading the event log and holds it through reset and replay. A concurrent accepted write is either included in that snapshot or runs after the rebuild. Trace import supplements the durable log, and failed imports roll back both the log and projection.

If replay cannot reconstruct an existing memory ID, rebuild rolls back and reports the missing-history condition. Older user-brain versions wrote some rows without accepted events; the guard preserves those rows but does not fabricate historical provenance. Back up such a brain and recover its missing events or migrate its legacy rows before rebuilding. This release prevents erasure; it does not provide a complete legacy-history migration.

Compaction is not archival. Automatic forgetting uses the separate reversible archive/restore lifecycle. Explicit invalidated-row purging still removes materialized audit rows; their durable events remain and replay can reconstruct them.
