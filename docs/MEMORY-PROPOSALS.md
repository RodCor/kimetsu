# Personal Memory Auto-Generation Plan

Status: **planning only**. The pieces are spec'd; nothing in this document is implemented yet beyond the existing `memory_proposals` table and the manual `kimetsu brain memory accept|reject` CLI.

The point of this document is to design `MemoryProposal` so it produces *transferable, personal* memories — preferences, conventions, workflow rules — instead of bug-specific trivia. Bug-specific facts ("the rectangle area function returned width+height") are noise; the brain only earns its keep when it captures things that help on the *next* unrelated task.

## What "personal" means here

Three concrete categories qualify; everything else gets rejected at the prompt level.

| Category | Scope | Examples |
|---|---|---|
| `preference` | global_user | "Prefer `rg` over `grep` for code search.", "Use Rust 2021 edition for new crates.", "Default shell is PowerShell on Windows." |
| `convention` | repo / project | "Fallible lookups are `find_*`; infallible are `get_*`.", "Tests live in `tests/`, not `src/`.", "Use `Result` over `unwrap` in this codebase." |
| `failure_pattern` | repo | "When `cargo test` reports `error: linking with link.exe failed`, run `cargo clean` first." |

Explicit non-categories:
- A specific function or file edited in this run (too specific)
- A specific bug's root cause (the bug is already fixed)
- "Project uses Rust" or "Project uses TypeScript" (trivially derivable from manifests)

## Pipeline integration

`MemoryProposal` becomes a real wired stage between `Review` and `FinalReport` (see MVP.md `## Coding Pipeline`). Currently the stage exists in the `CodingStage` enum but the pipeline jumps from Implementation/Verification straight to FinalReport.

```
... -> Verification -> Review -> MemoryProposal -> FinalReport
                                     |
                                     +--> [model proposes 0..N memories]
                                     +--> writes memory.proposed events
                                     +--> persists rows to memory_proposals
```

Triggers:
- Stage runs only when the run reaches `run.finished` (no proposals from failed runs in v0.1).
- Proposals are never auto-accepted. They land as `ProposalStatus::Pending`.

## Prompt design

The MemoryProposal stage runs a single model turn with no tools. The prompt is intentionally narrow.

```
System:
You are the MemoryProposal stage of Kimetsu. Review this completed run and
propose zero or more *personal memories* that would help on a similar future
task. A personal memory must be transferable: it applies beyond this
specific bug, file, or function.

Acceptable categories:
- preference: a user preference (tools, languages, style).
- convention: a codebase or project convention (naming, layout, design rules).
- failure_pattern: a recurring failure with a known mitigation.

Reject (do not propose):
- Anything that names this specific bug, function, or file path.
- "The codebase uses Rust" or other trivially-derivable facts.
- Memories whose only support is one observation in this run unless you
  explicitly say `confidence: 0.4` or lower.

Output exactly one JSON object on its own line, no prose, no markdown:

{
  "proposals": [
    {
      "scope": "global_user" | "project" | "repo",
      "kind":  "preference" | "convention" | "failure_pattern",
      "text":  "<single sentence; no proper nouns from the run>",
      "rationale": "<one line: what in the trace supports this>",
      "confidence": 0.0..1.0
    }
    , ...
  ]
}

Empty array is valid; emit { "proposals": [] } when nothing transferable
emerged.
```

User payload contains:
- the original task
- the final PatchPlan (rationale + expected_outcome only — not the diff)
- the tool catalog the run used
- the verification command(s) and their pass/fail status
- a digest of the run's tool-call sequence (10–20 entries, summarized)
- the existing accepted memories at run start (so the model doesn't repropose them)

The user payload deliberately *excludes* the actual diff and changed-file content. We don't want the model proposing memories rooted in the specific code it just touched; we want it generalizing from the *shape* of what it did.

## Schemas

`MemoryProposal` already exists in MVP.md. Confirming the shape:

```rust
MemoryProposal {
    proposal_id: Ulid,
    run_id: Ulid,
    proposed_at: DateTime<Utc>,
    scope: MemoryScope,            // global_user | project | repo
    kind: MemoryKind,              // preference | convention | failure_pattern
    text: String,
    rationale: String,
    proposed_confidence: f32,      // 0..1
    source_event_ids: Vec<Ulid>,   // events from this run that justify the proposal
    status: ProposalStatus,        // Pending | Accepted | Rejected
}
```

`source_event_ids` is the agent's claimed evidence; we store it for traceability so a human reviewing a proposal can read the supporting events without scrolling the whole trace.

The `memory_proposals` table already has the right columns (added in 3a). What's missing is the writer that emits `memory.proposed` events from this stage.

## Filtering personal vs general

Personal-only filtering happens at three layers, in order of strictness:

1. **Prompt** (above) instructs the model to skip non-transferable proposals.
2. **Parser-level rejection** drops proposals whose `text` contains:
   - paths from the run's PatchPlan (`files_to_modify`, etc.)
   - identifiers the agent edited (we have the diff; extract `pub fn <name>` and reject memories quoting those)
   - test names from `verification_commands`
3. **Dedup** rejects proposals whose `normalized_text` overlaps an existing accepted memory of the same `kind` and `scope`. Reuses the existing `normalized_text` rules from MVP.md.

Layer 1 is the cheap baseline; layers 2-3 are the safety net for prompt non-compliance.

## CLI surface

The acceptance commands already exist; what's missing is filtering and richer display.

```
kimetsu brain memory proposals
  [--scope global_user|project|repo]
  [--kind preference|convention|failure_pattern]
  [--from-run <run_id>]
  [--min-confidence 0.5]
  [--limit 20]

kimetsu brain memory accept <proposal_id>
  [--scope <override>]                # promote to a different scope on accept
  [--confidence <override>]           # default carries over from proposal

kimetsu brain memory reject <proposal_id>
  [--reason "<short>"]                # stored on the rejected row for later analysis
```

The default `proposals` list shows: `proposal_id | scope | kind | confidence | text` plus a one-line rationale. `--verbose` shows source_event_ids and the run that produced them.

## Bench integration

The current bench seeds warm memory by calling `project::add_memory` directly. Once auto-generation lands, the bench gets a fourth mode: `brain_on_auto_warm`.

```
brain_off              broker disabled, no memory
brain_on_cold          broker on, no memory
brain_on_warm          broker on, hand-seeded memory (current)
brain_on_auto_warm     broker on, memory seeded from a prior run's accepted proposals
```

Procedure for `brain_on_auto_warm`:
1. Run the *seed* task (`seed_task` in `BenchTask`) in `brain_on_cold` mode.
2. After that run finishes, take its proposals and auto-accept any whose
   `proposed_confidence ≥ 0.7` and whose category is `preference` or
   `convention` (failure_pattern is more risky for auto-accept).
3. Run the *follow-up* task (`followup_task`) with those memories active.

This mode is the **honest** test of the brain claim — no human curation, the brain only carries what the agent itself proposed.

## Acceptance heuristics (for `--auto-accept`)

If the user wants `kimetsu brain memory accept --auto` for the bench (or a future "trust the brain" flag), the heuristics:

| Condition | Auto-accept |
|---|---|
| `kind == preference` AND `scope == global_user` AND `confidence >= 0.8` | yes |
| `kind == convention` AND `scope == repo` AND `confidence >= 0.7` | yes |
| `kind == failure_pattern` | no — always require human review |
| `text` mentions specific identifiers (case-sensitive match in `files_to_modify`) | no |
| Duplicates of an existing memory (overlap detected) | no |

`--auto-accept` is opt-in per-invocation; it never runs from `kimetsu run coding` automatically.

## Risks and mitigations

1. **Memory leak** — a verbose model produces 5–10 proposals per run; the proposals table balloons. Mitigation: dedup at insertion time; `kimetsu brain memory proposals --status pending` is the default view; expire pending proposals older than 30 days.
2. **Bad memories degrade future runs** — a wrong convention memory makes warm runs *worse* than cold (we saw this happen with `rust_function_renamed` in the bench). Mitigation: track `use_count` and a simple usefulness signal (was a memory in the broker capsule set when a run succeeded). Memories whose use_count is 0 over N runs surface as candidates for `kimetsu brain memory invalidate` (deferred to v0.2).
3. **Privacy / leakage** — the model sees prior tool outputs that may contain sensitive content. Mitigation: the existing shell-output redaction policy already runs over stderr/stdout before they reach the model. Also: explicitly forbid the proposal prompt from including any `text` containing what looks like an API key, path-to-secret, or environment variable name. Reject at parse time.
4. **Over-personalization** — accepting many `global_user` memories creates a heavy default prompt that biases every future run. Mitigation: cap broker output of `global_user` memories per request via a configurable `[broker.weights.global_user_max_capsules]`. Default 3.

## Phasing

| Phase | Scope | Status |
|---|---|---|
| **MP-1** | Wire the MemoryProposal stage. Single-turn no-tool prompt. Parse and persist proposals. No auto-accept. | shipped (ebfd3dd) |
| **MP-1.5** | Prompt rewrite + threshold/default tuning to elicit useful proposals. | shipped (4e7696b) |
| **MP-1.6** | Bench measurement integrity: seed_task seeding, preserved seed traces, accepted-memory logging, injected-capsule logging, plan-create existence guard. | shipped (872129e) |
| **MP-1.7** | `brain_on_auto_warm_no_memory` ablation mode + `failure_category` in report. Isolates memory's contribution from prior_run's. | shipped (5c1e895) |
| **MP-2** | Acceptance UI: `--scope`, `--kind`, `--from-run`, `--min-confidence` filters. Rejection reason capture. | shipped (3ff9492) |
| **MP-3** | Bench `brain_on_auto_warm` mode + per-bench auto-accept heuristics. The first falsifiable test that the brain learns from runs. | shipped (4956257) |
| **MP-1.8 (deprioritized)** | ~Per-kind memory relevance gate before retrieval injection.~ The MP-1.7 ablation showed lexical relevance does not predict usefulness — memories at `rel=1.00` still hurt. Replaced by MP-4. | dropped |
| **MP-4** | Memory usefulness tracking. New `context.injected` event; `usefulness_score` column; broker scoring bias; auto-accept shadowing against low-usefulness existing memories; `kimetsu brain memory invalidate`. Details in [MEMORY-USEFULNESS.md](MEMORY-USEFULNESS.md). | next |
| **MP-5** | Confidence decay and pending-proposal expiry. | future |

MP-1 was ~150 lines of pipeline + ~50 of prompt. MP-2 was CLI ergonomics. MP-3 was the auto-warm bench mode. MP-1.5 and MP-1.6 were iteration on the prompt and measurement after observing the bench. MP-1.7 was the un-cooked ablation test that revealed the brain's wins so far come from `prior_run` capsules, not from accepted memories. MP-4 is the structural fix the ablation pointed at: outcome-weighted retrieval.

## Open questions

1. **Should MemoryProposal run on failed runs?** v0.1 says no. But failed runs often produce the most useful `failure_pattern` memories. Argument for yes: capture "this approach didn't work" as a memory. Argument for no: the model just failed; trusting its meta-cognition seems risky.
2. **How does the broker score auto-generated memories vs hand-added ones?** Currently `confidence` flows through unchanged. We could weight hand-added (1.0 confidence by default) higher than auto-accepted (carries over agent's claim, often 0.5–0.8). Probably no change needed — confidence already captures it.
3. **How is `source_event_ids` validated?** The agent claims these support the proposal. Do we cross-check that those events exist in the run's trace.jsonl? Yes, at parse time — reject the proposal if any cited event_id doesn't exist or doesn't belong to this run.
4. **Cross-project propagation.** A `global_user` memory accepted in project A surfaces in project B's runs (per scope precedence: `repo > project > global_user`). Is this what we want? Yes — that's the entire point of `global_user`. But the user should know that accepting a preference in project A affects all future runs in all projects.
