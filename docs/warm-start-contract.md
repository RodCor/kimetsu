# Warm-start delivery

Warm starts gather current inputs and assemble the bounded rule-based digest for delivery. Corrections, retirement, future starts and expiry apply to both digest facts and standing preferences. Matching cached text only avoids an unchanged disk write; it never replaces the freshly assembled result. This also prevents separate text/metadata cache publishers from mixing generations in model output. The synchronous database read makes no constant-time scaling claim.

The shared repo digest contains facts and manifests. Task titles and progress appear only in the caller-selected task/session/worktree resume lane. Global preferences use the normal read-only user-brain opener, including project and environment opt-outs.

Stdio MCP retains delivered warm-start keys for the latest 256 canonical workspace/identity pairs. Keys larger than 4096 combined path/identity units are not cached. An evicted or oversized key may receive a later repeat; all output remains subject to final serialization admission. This bounds process cache growth. The stdio request loop is sequential; the remote server uses its own session policy.

MCP prepares warm text without recording delivery, then marks the lane and records warm attribution only if the final budgeted response contains that block. A block omitted for space can be retried with a larger budget. Warm content is not inferred to be verified, and textual warm material does not receive fabricated capsule-based outcome credit.

BrainBenchmark's paired retrieval runs explicitly disable warm starts and ambient augmentation. Their query gold labels cover capsules, while warm-start validity, identity separation and omission/retry behavior are covered by integration regressions. Benchmark retrieval costs therefore exclude this separate opening-context service.
