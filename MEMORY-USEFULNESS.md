# Memory Usefulness Tracking (MP-4)

Status: **planning only**. This is the structural fix the MP-1.7 ablation pointed at. Nothing in here is implemented yet.

## Why this is the next move

The MP-1.7 bench (`01KRAJGKET4XNSNNN0CA12GVGN`) ran five modes side by side and produced one inconvenient finding:

```
                                success  turns   cost     plan_q
brain_off                       0%        77    $2.46    0.52
brain_on_cold                   94%       48    $1.49    0.93
brain_on_warm                   94%       45    $1.38    0.95
brain_on_auto_warm              94%       52    $1.85    0.87
brain_on_auto_warm_no_memory    94%       42    $1.38    0.85
```

`brain_on_auto_warm_no_memory` does the same seed → auto-accept → followup loop as `brain_on_auto_warm`, but the bench wipes the `memories` projection between phases. **It matches auto_warm's success rate at lower cost.** Per task:

- `rust_area_bug`: memory **helped** (8 turns / $0.28 vs 12 turns / $0.475)
- `rust_module_namespacing`: tie
- `rust_function_renamed`: memory **rescued** the run (no-memory variant hit a `gate.failed{patch_plan}` early-exit)
- `rust_two_file_bug`: memory **hurt** (failed at Verification after 19 turns / $0.86; no-memory variant succeeded in 10 turns / $0.36)

Memory's net contribution across the bench is **zero on success, negative on cost.** Two memories that helped, one that hurt, one tie. Lexical relevance does not predict which side a given memory will fall on — the hurting memory in rust_two_file_bug had `rel = 1.00`.

This means **MP-1.8 (relevance gate) is the wrong fix**. Filtering low-relevance memories doesn't help when high-relevance memories are the problem. We need a different signal: *did this memory actually correlate with successful outcomes the last N times we used it?*

MP-4 is that signal.

## Design contract

Track per-memory outcome correlation. Use it three ways:

1. **Broker scoring bias** — at retrieval time, multiply a memory capsule's score by a usefulness factor. Memories that have helped in past runs surface more; memories that have hurt surface less or not at all.
2. **Auto-accept policy** — reject re-acceptance of proposals whose `normalized_text` looks like an existing low-usefulness memory. Stops the agent from re-proposing the same bad convention every seed run.
3. **Human triage surface** — `kimetsu brain memory list` shows `usefulness_score / use_count`, so the operator can `kimetsu brain memory invalidate <id>` on memories that have demonstrably degraded runs.

## Schema additions

`memories` table gains a new column:

```sql
usefulness_score REAL NOT NULL DEFAULT 0.0
```

`use_count INTEGER` already exists; v0.1 never increments it. MP-4 will.

Idempotent migration via the existing `add_column_if_missing` helper (the same pattern MP-2 used for `decided_reason`). No `kimetsu brain rebuild` required.

## New trace event: `context.injected`

The projector cannot decide "this memory was in the context of run X" from existing events. `model.requested` records `tool_names` but not capsule IDs. `stage.completed` for `context_retrieval` records capsule counts but not IDs. We need a typed record of "these are the memory IDs the broker surfaced into stage Y of run Z."

New event kind:

```
kind: "context.injected"
payload: {
    stage: "patch_plan" | "localization" | ...,
    capsule_ids: [Ulid],
    memory_ids: [String],            // subset of capsule_ids whose handle is "memory:<id>"
    prior_run_ids: [String],         // subset whose handle is "run:<id>"
    file_paths: [RepoRelPath],       // subset whose handle is "file:<path>"
    manifest_paths: [RepoRelPath]
}
```

Emitted by `ContextRetrieval` stage immediately after broker returns a bundle, *before* the bundle is rendered into a prompt. One event per stage that retrieves context (Localization, PatchPlan, Implementation, Verification, Review).

Event kinds are still a closed set in v0.1; bump `schema_version` per the MVP.md rule.

## Outcome attribution rules

When the projector applies a `run.finished` or `run.failed` event, it walks back through the run's events and finds every `context.injected`. For every `memory_id` that appears in any of them:

```
on run.finished:
  memories.usefulness_score += 1
  memories.use_count        += 1

on run.failed (category != "Gate"):
  memories.usefulness_score -= 1
  memories.use_count        += 1

on run.failed (category == "Gate"):
  # graceful early-exit; doesn't reflect on the memory's quality
  no update

on run.aborted:
  no update
```

A memory injected into multiple stages of the same run counts **once per run**, not per stage. We are measuring "did the run that saw this memory succeed?", not "did the model's call that used this memory succeed?". Per-call attribution is post-MVP.

Run-scoped memories (`scope: run`) are excluded; usefulness only tracks persistent scopes.

## Broker score bias

The current scoring (per MEMORY-PROPOSALS.md and `context.rs`):

```
score = 0.50 * relevance + 0.20 * confidence + 0.20 * freshness + 0.10 * scope_weight
```

MP-4 introduces a usefulness factor applied **only to memory capsules** (not `repo_file`, `repo_manifest`, `prior_run`, etc.):

```
usefulness_ratio = if use_count >= 3 {
    (usefulness_score + use_count) / (2 * use_count)   // maps -use_count..+use_count to 0..1
} else {
    0.5   // small-sample default; treat as neutral
};

memory_score = base_score * (0.5 + usefulness_ratio)
             = base_score * 0.5            // proven harmful (-use_count)
             ...
             = base_score * 1.0            // neutral / small sample
             ...
             = base_score * 1.5            // proven helpful (+use_count)
```

`use_count >= 3` is the small-sample threshold; below it we treat memory as neutral so a new memory has a fair chance to demonstrate value before being penalized or boosted. The (-50% .. +50%) envelope is bounded so a single memory can't dominate the budget either direction.

The constants live in `[broker.usefulness]` in `project.toml` so they can be tuned per-project:

```toml
[broker.usefulness]
small_sample_threshold = 3
multiplier_min = 0.5
multiplier_max = 1.5
```

## Auto-accept policy update

The existing policy (preference + global_user >= 0.75, convention + repo|project >= 0.7) stays. MP-4 adds one extra check at the *start* of the policy:

```
shadowed = existing memories where:
  scope == proposal.scope
  AND kind == proposal.kind
  AND tokens_jaccard(normalized_text, existing.normalized_text) >= 0.5
  AND existing.use_count >= 3
  AND existing.usefulness_score / existing.use_count < -0.2

if shadowed.non_empty():
    return AutoAcceptDenied {
        reason: "shadowed_by_low_usefulness",
        shadow_id: shadowed[0].id,
    }
```

Reads as: *"if a proposal is similar to a previously-accepted memory that has hurt more runs than it helped, do not auto-accept this rephrased version."* Lets human triage keep the door open (`kimetsu brain memory accept` still works), but the bench's auto-warm mode will stop re-injecting the same bad pattern every cycle.

The 0.5 Jaccard threshold is the placeholder; needs tuning against the bench's actual proposal stream.

## CLI surface additions

`kimetsu brain memory list` output gains two columns (already has `confidence` and `use_count`):

```
<memory_id> [<scope>:<kind> confidence=X.XX uses=N usefulness=+M (M/N=R.RR)] <text>
```

`R.RR` is `usefulness_score / use_count` to two decimals, or `--` when `use_count == 0`.

New command:

```
kimetsu brain memory invalidate <memory_id> [--reason "<short>"]
```

Implementation: emits a `memory.invalidated` event (new kind, closed-set bump). The projector flips `memories.invalidated_at` (new column) so the broker stops surfacing it. This is the manual override for memories the user wants gone even when usefulness signal is small-sample.

`memory.invalidated` events live alongside `memory.accepted` and `memory.rejected` in the canonical trace.

## Bench instrumentation additions

The bench report grows one section:

```
## Memory Usefulness (this bench run)

| memory_id | scope/kind | text | uses_in_bench | helped | hurt | net | usefulness_ratio_persisted |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| ... | repo/convention | "When tests reference..." | 3 | 2 | 1 | +1 | +0.33 |
```

This is the human-facing answer to "why did memory X help in some tasks and hurt others?" The bench tracks per-run injection (already does, via `injected_capsules` from MP-1.6) and joins it against terminal_kind to produce this table. Pure derived data; no new event kinds needed beyond `context.injected`.

## Risks and mitigations

1. **Cold-start: a new memory gets a bad first run and is suppressed forever.**
   Mitigation: `use_count >= 3` small-sample threshold. Memories under 3 uses get neutral treatment.

2. **A genuinely useful memory hurts one outlier task and gets penalized.**
   Mitigation: the (-50% .. +50%) envelope on memory_score multiplier. A memory needs to consistently hurt to lose half its score, not be hurt by a single noisy run.

3. **Gate failures shouldn't blame memories.**
   Mitigation: the `failure_category == "Gate"` exclusion. A `run.failed{Gate}` is the plan-create existence guard catching a contradiction; the memory in context didn't cause it (and shouldn't be punished for it).

4. **Auto-accept's shadowing check turns into a runaway "everything is shadowed" rejection.**
   Mitigation: Jaccard threshold of 0.5 (high overlap requirement) AND `use_count >= 3` on the shadow (small-sample protection) AND a negative usefulness floor (`<-0.2`). Three filters before rejection.

5. **Cross-project propagation of usefulness for `global_user` memories.**
   A global_user memory's usefulness aggregates across all projects. A memory that helps in project A and hurts in project B nets out. This is intentional for v0.1; per-project usefulness adds a join we don't need yet.

6. **`memory.invalidated` adds a fourth memory event kind.**
   Mitigation: spec it now, accept the schema_version bump; alternative is wedging the state into `memory.rejected` which is wrong semantically (rejecting a proposal vs invalidating an accepted memory are different).

## Phasing

| Phase | Scope |
|---|---|
| **MP-4a** | `context.injected` event emission from the pipeline; schema column adds; projector updates `usefulness_score` on terminal events. No retrieval changes yet — pure measurement. |
| **MP-4b** | Broker scoring bias applied to memory capsules using the new `usefulness_ratio`. |
| **MP-4c** | Auto-accept shadowing check against low-usefulness existing memories. |
| **MP-4d** | `kimetsu brain memory invalidate` command + invalidated_at column + the broker filtering invalidated memories from retrieval. CLI `list` shows usefulness. |
| **MP-4e** | Bench report `## Memory Usefulness` section + bench validation. |

MP-4a is the foundation; the others can land independently once measurement exists. MP-4e is the validation: re-run the 5-mode bench and look for `brain_on_auto_warm` cost to converge toward `brain_on_auto_warm_no_memory` from below (memories that hurt get suppressed, so cost drops; memories that help get boosted, so success stays at 94%+).

## What the next bench will tell us

Three scenarios after MP-4 lands:

1. **Memory is now load-bearing**: `auto_warm` beats `auto_warm_no_memory` on cost and matches on success. The usefulness signal correctly suppressed the rust_two_file_bug-killing memory and amplified the rust_area_bug-helping one.
2. **Memory is still net-neutral but cheaper**: cost converges to no-memory baseline, success rate stays 94%. Means the signal damped noise but didn't extract positive value. Still a win on the cost axis.
3. **Memory regresses**: usefulness signal is too noisy at v0.1 sample sizes (each fixture runs once per bench). Means MP-4 needs more data per memory to be useful — fold in per-stage attribution or longer-running brains.

Scenario (3) is the explicit kill criterion: if MP-4 lands and the bench shows no improvement over `auto_warm_no_memory`, the personal-memory pipeline is *deprioritized for v0.1*. The brain's documented value moves entirely to `prior_run` capsules and the broker's grounding effect. Memory becomes a future-iteration concern.

## Out of scope for MP-4 (deferred)

- Per-stage memory attribution (which stage was the memory actually consulted in)
- Per-call attribution (which model.requested actually mentioned the memory in its prompt)
- Time-decay on usefulness (older outcomes count less)
- Cross-memory interaction effects (memory X helps only when memory Y is also in context)
- Embedding-based shadow detection (Jaccard tokens only in v0.1)
