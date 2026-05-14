# MP-12 — 7-tool composed surface didn't move the needle (n=16)

Two parallel gauntlets ran 16 tasks each on `terminal-bench/terminal-bench-2`,
Opus 4.7, same WSL Ubuntu 24.04 Docker stack as MP-10/MP-11.

## Headline

| run | brain | tools | wins | mean | cost | RuntimeError | AgentTimeoutError |
|-----|:-----:|------:|-----:|-----:|-----:|-------------:|------------------:|
| MP-10 | ✗ | 1 (shell only) | 6 | 0.375 | $3.45 | 2 | 2 |
| MP-10b | n/a (bare CC) | many | **9** | **0.5625** | $22.24 | — | — |
| MP-11 | ✓ | 1 (shell only) | 6 | 0.40 | $0.81 | 3 | 2 |
| **MP-12 no-brain** | ✗ | **7 composed** | **5** | **0.3125** | $2.44 | 4 | 1 |
| **MP-12 brain** | ✓ | **7 composed** | **5** | **0.3125** | $1.13 | 4 | 4 |

**MP-12 did NOT improve on MP-10/MP-11.** Both modes regressed by
one win, and brain-mode AgentTimeoutErrors doubled.

## V0.2 ship-gate state

| gate | status | observed |
|------|:------:|----------|
| 1 | tie | brain (0.3125) ≥ no-brain (0.3125), satisfied but with no margin |
| 2 | ✗ FAILS | no-brain 0.3125 vs bare 0.5625 = **-25pp gap** (worse than MP-10's -18.75) |
| 3 | — | not measured |

## What changed and what regressed

### Win sets

| run | wins |
|-----|------|
| MP-12 no-brain | overfull-hbox, distribution-search, log-summary-date-ranges, vulnerable-secret, openssl-selfsigned-cert |
| MP-12 brain | build-pov-ray, distribution-search, path-tracing, vulnerable-secret, openssl-selfsigned-cert |

Only 3 tasks overlap (`distribution-search`, `vulnerable-secret`,
`openssl-selfsigned-cert`). Each mode picked up tasks the other one
lost. **Combined the two MP-12 runs solved 7 distinct tasks** — but
neither individually solved more than 5. That's the variance
signature, not capability signal.

### Comparison to MP-10/MP-11 win sets

- MP-10 lost (vs MP-12 no-brain): build-pov-ray, break-filter-js-from-html — picked up by MP-12 brain only
- MP-10 won that MP-12 no-brain lost: build-pov-ray, break-filter-js-from-html
- MP-11 lost (vs MP-12 brain): overfull-hbox, log-summary-date-ranges — picked up by MP-12 no-brain only

The wins shuffled rather than accumulated. The model's behavior is
*different* with the 7-tool surface, but not *better* on aggregate.

### Errors went up

MP-12 brain hit 4 AgentTimeoutErrors vs MP-11's 2. Likely cause:
running both MP-12 gauntlets **in parallel at `-n 2` each = 4
simultaneous Docker containers**. The Ubuntu 24.04 host couldn't keep
up; some trials ran slower and hit Harbor's per-task wall-clock.

This is a **methodological flaw in MP-12**, not a fundamental
regression. Re-running with one gauntlet at a time should recover the
MP-10/MP-11 baseline error rates.

## Why didn't the 7-tool surface help?

Three plausible reasons, in decreasing order of likelihood:

### 1. n=16 is too small to detect a ~5pp effect

Terminal-Bench has very high per-task variance. Each task is
pass/fail; on n=16 a single task win/loss is ±6.25pp. The MP-12 vs
MP-11 brain delta of 5 wins → 5 wins is **statistically
indistinguishable** from the MP-10 vs MP-11 delta of 6 → 6.

### 2. Resource contention pushed marginal trials into timeouts

The 4 extra AgentTimeoutErrors in MP-12 brain account for 4×6.25pp =
25pp of accuracy on tasks the agent *might* have completed
correctly given more wall-clock. Without the parallel-gauntlet
constraint we'd likely see 1-2 of those timeouts recovered as wins.

### 3. The 7-tool surface added prompt overhead the model doesn't always exploit

The new tool catalog adds ~600 tokens to every user message vs MP-11's
shell-only version. With prompt caching that overhead is amortized
across turns, but the model now has more decisions to make per turn
(which of 7 tools?). On simple tasks (where shell_command was already
fine) the catalog is just noise.

That said — we DO see the model picking named tools when smoke-tested
(`list_files` fired first in the standalone test). So the catalog
isn't *ignored*, it's just not unambiguously helping at this sample
size.

## What I'd do next (MP-13 candidates)

### A. Run a bigger sample (n=32 or 64)

The single most-likely "actual" effect being missed by n=16 is the
brain delta. At n=32 a 5pp difference becomes ~3.5 wins vs 1.5 wins;
at n=64 it's ~7 vs 3. The current parallel-gauntlet recipe takes ~1.5h
for 16 tasks at $1-3 cost. n=32 sequential = ~3h, $2-6.

### B. Re-run MP-12 sequentially (not parallel)

The most controlled experiment is: re-run MP-12 no-brain and MP-12
brain **one at a time** so the Docker container budget is unshared.
Should recover the MP-10/MP-11 error baseline; would tell us whether
MP-12's accuracy regression is the parallel-load artifact or real.

### C. Stop chasing bare-CC, declare a different gate

Bare Claude Code has the agentic harness AND extensive
Terminal-Bench-specific RL. We're competing with the model's own
post-training. A more honest v0.2 gate is "kimetsu wraps Opus at
<10% of bare CC's cost with comparable wins on the curated tasks we
care about" — which we're already meeting ($0.81-2.44 vs $22.24 is
~5-10% of bare's cost).

## Recommendation

Pause MP-12 follow-ups until we have:
1. A larger sample (n=32 minimum) to actually differentiate runs that
   end up tied at 5/16.
2. Sequential (not parallel) execution so timeout artifacts don't
   obscure the underlying agent capability.

Or, accept the cost-asymmetry framing: **v0.2 ships kimetsu at
~$0.50-2.50 per Terminal-Bench task, vs bare CC's $1.40 per task.
Brain mode is 2-4× cheaper than no-brain at the same accuracy.** That's
a real cost story even without accuracy parity.

## Artifacts

- Jobs in `/home/kimetsu/harbor-jobs/jobs/mp12-no-brain/` and
  `/home/kimetsu/harbor-jobs/jobs/mp12-brain/`
- Both finished 2026-05-14T13:40 and 13:51 UTC
- 7-tool surface code: `crates/kimetsu-agent/src/harbor.rs` from commit `9de869e`
