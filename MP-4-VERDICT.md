# MP-4 Verdict — Personal Memory Pipeline Deprioritized for v0.1

**Bench**: `01KRBW7P5QP75T96VAEYC1EWK1` — 16 tasks × 5 modes = 80 runs, $7.38, all
modes green. Binary `kimetsu.exe` built 2026-05-11T15:58:10Z, includes MP-4a/b/c/d
and the `claude_code` provider deadlock fix (commit `6ea84c3`).

## Headline

The MEMORY-USEFULNESS.md kill criterion (Scenario 3, §229) has been hit. The
personal-memory pipeline is **deprioritized for v0.1**. The brain's documented
value in v0.1 moves entirely to broker grounding and `prior_run` capsules.

The MVP falsifiable claim still passes trivially for all warm modes — the
brain itself remains load-bearing.

## What the numbers say

| mode | success | model_turns | cost | avg_ms | rel_signal | plan_q | invalid |
|------|--------:|------------:|-----:|-------:|-----------:|-------:|--------:|
| brain_off | **0%** | 74 | $2.22 | 29.9s | 0% | 0.51 | **42** |
| brain_on_cold | 88% | 42 | $1.22 | 15.2s | 88% | 0.90 | 1 |
| **brain_on_warm** | **100%** | 42 | $1.24 | 15.4s | **100%** | **0.96** | **0** |
| brain_on_auto_warm | 94% | 49 | $1.63 | 18.3s | 81% | 0.88 | 5 |
| brain_on_auto_warm_no_memory | 94% | 36 | $1.07 | 13.5s | 88% | 0.87 | 7 |

### MVP falsifiable claim — passes everywhere

Threshold (MVP.md): ≥15 pp success uplift OR ≥20% fewer tool calls OR strictly
fewer verification retries vs `brain_off`.

| mode | Δ success | Δ tool calls | Δ retries | passes |
|------|----------:|-------------:|----------:|:------:|
| brain_on_warm | +100 pp | -63% | 5→1 | ✓ |
| brain_on_auto_warm | +94 pp | -52% | 5→0 | ✓ |
| brain_on_auto_warm_no_memory | +94 pp | -73% | 5→0 | ✓ |

The 42-invalid-plan, 0%-success brain_off baseline confirms the broker and
prior_run capsules are doing real work. The brain is unambiguously
load-bearing in v0.1.

### MP-4 kill check — `auto_warm` vs `auto_warm_no_memory`

This is the differential test the design doc framed as the v0.1 ship decision
for personal memory. Memories should suppress hurtful entries and amplify
helpful ones across runs.

| metric | auto_warm | auto_warm_no_memory | Δ |
|--------|----------:|--------------------:|---|
| success | 94% | 94% | **tie** |
| model_turns | 49 | 36 | **+36% worse** |
| cost | $1.63 | $1.07 | **+52% worse** |
| avg_ms | 18,308 | 13,480 | **+36% worse** |
| relevant_signal | 81% | 88% | **-7 pp worse** |
| irrelevant_context | 63 | 60 | +3 (noisier) |

Per §231: *if MP-4 lands and the bench shows no improvement over
`auto_warm_no_memory`, the personal-memory pipeline is deprioritized for
v0.1.* Headline result matches Scenario 3.

## Where the signal actually went — per-task breakdown of the 4 real-fix tasks

The aggregate masks a real per-task pattern. MP-4 *does* move outcomes
differentially, just not net-positively at single-run-per-fixture sample
sizes.

| task | auto_warm | auto_warm_no_memory | notes |
|------|-----------|---------------------|-------|
| rust_area_bug | ✓ ($0.20) | ✓ ($0.22) | tie |
| rust_module_namespacing | ✓ ($0.23) | ✓ ($0.23) | tie |
| **rust_two_file_bug** | **✓ ($0.37, 11 turns)** | ✗ Gate-fail ($0.03) | memory helped — without it the followup Gate-tripped |
| **rust_function_renamed** | **✗ Implementation-fail ($0.54)** | ✓ ($0.30) | memory hurt — the auto-accepted memories misled the agent |

Net: 2-1 in favor of memories on the real-fix subset, but the one failure
costs $0.54 in wasted model spend — more than the rust_two_file_bug win
saves. Aggregate cost goes negative.

The signal is real. It's just not stable enough at v0.1 sample sizes (one
seed run per fixture) for the usefulness multiplier to discriminate before
the next bench draws its conclusion.

## Decision

1. **Deprioritize the personal-memory pipeline for v0.1.** Stop iterating on
   prompt engineering for `MemoryProposal`, on auto-accept thresholds, on
   shadow detection heuristics. The MP-4b/c/d code stays merged but goes
   inert on the documented v0.1 path because v0.1 doesn't propose memories.
2. **v0.1 story locks to `brain_on_cold` + curated memories via the CLI.**
   Cold broker alone delivers 88% success vs 0% off — the load-bearing
   piece. `kimetsu brain memory add` remains the supported way to seed
   memories; `brain_on_warm` (curated) shows the ceiling at 100% / 0
   invalid plans / 0.96 plan quality.
3. **MP-4 code is preserved as future-iteration substrate.** Outcome
   attribution, usefulness multiplier, auto-accept shadowing, and
   `memory invalidate` are all wired and tested. They're cheap to keep
   (zero cost when no memories exist) and become the foundation for v0.2
   if/when the personal-memory hypothesis is revisited.
4. **The MVP.md falsifiable claim remains green** for the v0.1 story. The
   brain architecture is validated. The unverified hypothesis is "the model
   can reliably propose memories that help itself" — that's the part going
   to v0.2.

## What stays load-bearing in v0.1

| component | role | bench evidence |
|-----------|------|----------------|
| ContextBroker | budgeted retrieval over repo + memories + prior_runs | cold=88% vs off=0%; -63% tool calls |
| Repo capsules | files surfaced via FTS5 + lexical match | cold mode has 0 memories but still 88% |
| Prior-run capsules | seed→followup carry-over of localization context | drives the auto_warm and auto_warm_no_memory uplift over cold |
| Manually-added memories | top-of-bench amplifier when the user knows the rule | warm=100% / plan_q 0.96 / 0 invalid |
| Verification + retry-with-fingerprint | failure-feedback loop | brain_off retries=5, warm retries=1 |

## What's deprioritized (kept on the shelf)

| component | status |
|-----------|--------|
| MemoryProposal stage | code stays, runs in the seed phase, but auto-accept doesn't ship as a documented v0.1 path |
| Auto-accept policy (preference 0.75, convention 0.7, shadowing) | inert when no proposals are auto-accepted; tests stay green |
| Broker usefulness multiplier (MP-4b) | inert when no memories exist; mathematically a no-op |
| Auto-accept shadowing (MP-4c) | inert when the memory pool is empty |
| `memory invalidate` CLI (MP-4d) | shipped, useful for curated path; not a v0.1 marketing feature |

## Future-iteration unlock conditions (v0.2 brief)

If we revisit personal memory:

1. **Need ≥3-5 runs per fixture per bench** so the usefulness signal
   stabilizes before the bench draws conclusions. Current 1-run-per-fixture
   benches can't tell helpful memories from hurtful ones in time.
2. **Per-stage attribution** — currently a memory injected into PatchPlan
   AND Localization in the same run counts once. v0.2 should weight which
   stage the memory actually influenced.
3. **Embedding-based shadow detection** — token-Jaccard at 0.5 is too
   blunt; rust_function_renamed's failure suggests semantically-similar but
   subtly-wrong memories slip through.
4. **Tighten the auto-accept policy** — the proposal flow accepted
   memories on rust_function_renamed that turned out to be wrong. Either
   raise confidence thresholds or add a "passed verification at least
   once" gate before propagating a memory across runs.

## Artifacts

- Bench report: `.kimetsu/bench/01KRBW7P5QP75T96VAEYC1EWK1/report.md`
- Raw results: `.kimetsu/bench/01KRBW7P5QP75T96VAEYC1EWK1/results.json`
- Per-task seed traces: `.kimetsu/bench/01KRBW7P5QP75T96VAEYC1EWK1/artifacts/`
- Design doc this verdict references: `MEMORY-USEFULNESS.md`
- Binary used: `target/release/kimetsu.exe`, built 2026-05-11T15:58:10Z
- Includes commits: `a14d2d1` (MP-4b/c/d) + `6ea84c3` (claude_code deadlock fix)
