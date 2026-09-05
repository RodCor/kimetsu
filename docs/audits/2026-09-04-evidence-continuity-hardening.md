# Evidence and continuity repair

Citation records reliance on delivered context; successful-run attribution records observed association. Neither proves the memory is true or caused success. Origin penalties remain after citation/association; no verification channel was added. Public provenance audit groups expose `associated`, and externally sourced memories remain unvetted.

Context delivery persists exact IDs and hydrated revision bindings under an exposure event ID and a real run ID. MCP context returns that ID inside its final serialized byte budget. Citation and benchmark outcome APIs resolve the durable exposure rather than retrieving again after the task. Unknown outcomes and absent/unbound exposures produce no automatic memory credit. Mixed claim revisions in one run fail closed for automatic attribution. Query storage continues to honor learning.store_queries.

Schema v14 adds an optional work episode identity. An empty value is the legacy lane. Checkpoint/resume expose --task-id, with --session-id and --worktree-id aliases. Hook/MCP identity precedence is task_id, session_id, worktree_id; explicit identity requests never fall back to unrelated work. Episode writes and supersession are atomic within a lane and replayable.

Forgetting remains opt-in archival. Candidates are revalidated under the same writer lock, including claim revision and meaningful recency. `brain archives` lists archives and `brain restore <memory-id>` writes memory.restored. Restoration cannot reopen temporal expiry or revive invalid/superseded claims, and invalidity cannot be overwritten with an archive reason to evade that rule.

Manual conflict decisions now persist self-contained conflict.resolved events atomically with losing-side retirement. Rebuild preserves kept_new, kept_existing, and kept_both. Similarity is only a detection signal.

ROI constants remain nominal assumptions. Reports expose the model and assumption table, identify estimates, and keep recorded delivery cost units separate. UTF-8 byte bounds and heuristic token estimates are not measured model tokens or evidence of counterfactual savings. Existing association-based usefulness/confidence nudges remain heuristics.

Focused regression evidence and exact API signatures are recorded in tmp-tests/task-5-report.md. No paid generations, live-brain edits, global configuration changes, or performance claims were involved.
