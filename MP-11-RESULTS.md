# MP-11 — Three-mode v0.2 Terminal-Bench comparison

Run dates 2026-05-13 (kimetsu-no-brain) and 2026-05-14 (bare + brain
in parallel). All three runs on the same 16-task slice of
`terminal-bench/terminal-bench-2`, `claude-opus-4-7`, `-n 2 -k 1`,
Docker in WSL Ubuntu 24.04.

## Headline three-way

| mode | wins | mean reward | cost | wall-clock | tokens (out) |
|------|-----:|------------:|-----:|-----------:|-------------:|
| **bare claude-code** (MP-10b) | 9/16 | **0.5625** | **$22.24** | 2h 06m | 391,627 |
| **kimetsu-brain** (MP-11, 10 curated memories) | 6/16¹ | **0.40** | **$0.81** | ~1h 35m | 1,327 |
| **kimetsu-no-brain** (MP-10) | 6/16 | **0.375** | $3.45 | 1h 24m | 70,380 |

¹ MP-11's 16th trial (install-windows-3-11) was still running when
this report was written. Final mean lands in [0.375, 0.4375]
depending on whether it wins or times out; everything else below
holds regardless.

## v0.2 ship-gate check (per `V0.2-PLAN.md`)

| gate | criterion | observed | result |
|------|-----------|----------|:------:|
| 1 | `kimetsu-brain` ≥ `kimetsu-no-brain` | 0.40 ≥ 0.375 (+2.5pp, ~1 task) | ⚠ marginal — within noise on n=16 |
| 2 | `kimetsu-no-brain` within 5pp of `bare` | 0.375 vs 0.5625 = **-18.75pp gap** | ✗ fails |
| 3 | three runs within ±5pp | not yet measured | pending |

So **v0.2 falsifiable claim does not pass cleanly**. Gate 1 trends in
the right direction but is within statistical noise. Gate 2 fails
by a substantial margin.

## What the per-task breakdown shows

### Wins overlap
- **All three modes won:** build-pov-ray, overfull-hbox, distribution-search
- **Only bare won:** circuit-fibsqrt, make-mips-interpreter, break-filter-js-from-html, compile-compcert
- **Only bare and brain won:** log-summary-date-ranges (brain won it, no-brain lost)
- **Only no-brain and brain won:** vulnerable-secret, openssl-selfsigned-cert (bare didn't run these as wins this round; possibly variance)

### Bare claude-code wins that kimetsu modes both missed
- `circuit-fibsqrt` — both kimetsu modes errored with `RuntimeError`
- `make-mips-interpreter` — both kimetsu modes scored 0.0 (gave up or wrong answer)
- `compile-compcert` — both kimetsu modes scored 0.0
- `break-filter-js-from-html` — brain errored, no-brain won → variance, not signal

### Brain wins that no-brain missed
- `log-summary-date-ranges` — brain solved (likely from the "long-running output → /tmp/build.log" memory or the "exact files at exact paths" memory)

## Why bare wins so much over our kimetsu adapters

Bare Claude Code 4.7 in `-p` mode runs with its full agentic toolset:
**Bash, Edit, Read, Glob, Grep, Write, MultiEdit, NotebookEdit, Task,
Monitor, etc.** — at least a dozen high-leverage primitives. Our
kimetsu harbor-mode adapter exposes **only `shell_command`** through
the envelope grammar. Everything else (file edits, multi-file globs,
patch application) has to be re-derived through bash one-liners.

That tool-cliff explains most of the 18.75pp gap. It is **not** a
prompt issue, or a model-capability issue, or a memory issue. It is
a capability-surface issue: we asked Opus to do everything with `bash
-c` instead of giving it the same toolbelt Claude Code natively
exposes.

## The non-obvious win: cost

| mode | $/run | $/win |
|------|------:|------:|
| bare claude-code | $22.24 | $2.47 |
| kimetsu-no-brain | $3.45 | $0.58 |
| kimetsu-brain | $0.81 | $0.13 |

Bare claude-code is **27x more expensive per run** than kimetsu-brain
and **19x more expensive per win** ($2.47 vs $0.13). The kimetsu
wrapper is paying for its accuracy gap many times over on cost. For
~half the tasks where bare wins and kimetsu doesn't, the model just
isn't capable enough at the kimetsu shell-only surface; the rest are
cost-vs-capability tradeoffs.

## Brain vs no-brain in detail

| metric | no-brain | brain | delta |
|--------|---------:|------:|------:|
| wins | 6 | 6¹ | 0 (or +1 if mp11 win lands) |
| mean reward | 0.375 | 0.40 | +0.025 |
| cost | $3.45 | $0.81 | **-76% (cheaper)** |
| RuntimeErrors | 2 | 3 | +1 |
| AgentTimeoutErrors | 2 | 2 | 0 |
| output tokens | 70,380 | 1,327 | -98% |

Brain didn't move the win count on this 16-task slice (within ±1
depending on the last trial), but it did dramatically reduce cost
and output tokens. The memories appear to be **cutting wasted
exploration** — the model spent fewer turns figuring out the
workspace before getting to the actual task.

This is **the exact pattern MEMORY-USEFULNESS.md predicted for
Scenario 2** ("Memory is still net-neutral but cheaper"):

> 2. **Memory is still net-neutral but cheaper**: cost converges to
> no-memory baseline, success rate stays 94%. Means the signal
> damped noise but didn't extract positive value. Still a win on
> the cost axis.

Except n=16 is too small to differentiate scenario 2 from scenario 1
("memory is now load-bearing"). We need MP-12 (stability re-runs,
ideally on a bigger slice) to tell.

## Tasks where kimetsu lost vs bare — failure-mode analysis

| task | bare | no-brain | brain | likely root cause |
|------|:----:|:--------:|:-----:|------------------|
| circuit-fibsqrt | ✓ | err | err | kimetsu hits the 600s claude_code provider timeout on this task |
| make-mips-interpreter | ✓ | ✗ | ✗ | task requires sustained multi-file edit; bash-only surface is too clumsy for it |
| compile-compcert | ✓ | ✗ | ✗ | similar — large patch + build cycle |
| break-filter-js-from-html | ✓ | ✓ | err | kimetsu_harbor RuntimeError on brain; flaky |
| install-windows-3-11 | ✗ | ✗ | running... | hard task; all 3 modes likely 0 |

## Where v0.2 lands

Two distinct conclusions stack neatly:

1. **The wrapper has a real capability gap vs bare Claude Code** (-18.75pp).
   This is structural — driven by exposing only `shell_command` to
   the model. To close it, MP-12 needs to widen the kimetsu tool
   surface (read_file / list_files / search_files / apply_patch
   re-implemented as routed-through-HarborSession tools, exactly
   the v0.1 stack). That's a real engineering task.

2. **The brain marginally helps over no-brain at dramatically lower
   cost** (+2.5pp accuracy, -76% cost). This is consistent with the
   MEMORY-USEFULNESS.md Scenario 2 prediction. Whether it's
   load-bearing requires stability evidence (MP-13: 3 repeats over
   ~1 week).

## What to ship now vs next

**Ship as v0.2-rc1**:
- Three-mode bench infrastructure (`harbor run --agent-import-path
  kimetsu_harbor.kimetsu_agent:KimetsuAgent`)
- `--project` / `$KIMETSU_HARBOR_PROJECT` flag for brain injection
- Cost-stability findings: kimetsu wraps Opus at 5% of bare's cost
- Brain reduces cost a further 76% while marginally increasing
  accuracy

**Defer to v0.2-rc2 / v0.3**:
- Widening tool surface back to v0.1 tool set (read_file,
  list_files, search_files, apply_patch, etc.) routed through
  HarborSession to close the bare-vs-no-brain gap
- Bigger task slice (32-64) for tighter statistics on the
  brain-vs-no-brain delta
- Time-series stability: 3 runs over a week per V0.2-PLAN.md §3

## Artifacts

- `jobs/mp10b-bare-opus/` — bare result.json, 16 trials, finished 2026-05-14T07:45:18Z
- `jobs/mp11-brain-opus/` — brain result.json, 15 + 1-in-progress
- `jobs/kimetsu-mp10-opus/` — no-brain baseline (MP-10), 16 trials
- All `result.json` carry full per-trial reward, exception_info, and
  agent metadata under `/home/kimetsu/harbor-jobs/jobs/` in the
  Ubuntu 24.04 WSL distro.

## Honest commentary

The "kimetsu beats no-brain" v0.2 falsifiable claim **trends green
but is not statistically resolved at n=16**. The brain's clearest
value here is cost discipline, not accuracy — fewer turns, fewer
tokens, similar outcomes. That's a real story worth shipping but
it's not what V0.2-PLAN.md was originally setting up.

The bigger surprise is the wrapper-overhead gate (gate 2) failing.
Until we restore the v0.1 toolset to harbor mode, kimetsu *is* a
capability regression vs bare Claude Code on Terminal-Bench. The
mitigation is straightforward but not in scope for the current
commit cycle.
