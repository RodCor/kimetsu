# MP-15 — close residual gate-2 gap + lock gate-3 with a time series

MP-14d (`MP-14-RESULTS.md`) landed gate 1 with a clean +6.25pp brain
margin and pinned gate 3 to a three-run bullseye on no-brain.
Gate 2 still fails on the no-brain leg (-18.75pp vs bare CC) but
the brain leg closed it to -12.5pp. MP-15 is the residual-cleanup
phase: address the specific failure modes that remain, then run a
week of daily gauntlets to convert gate 3 from "same-day stable"
to a real time-series proof.

## What the MP-14d data actually pinned

The brain-vs-bare-CC gap is **3 tasks**, not 5 — kimetsu wins
`openssl-selfsigned-cert` that bare CC loses, so the net delta is
9 - 7 = 2 tasks. Of those:

| task | bare CC | MP-14d brain | root cause | MP-15 lever |
|---|:---:|:---:|---|---|
| `circuit-fibsqrt` | ✅ won | RuntimeError | 600s claude_code provider timeout — model was still actively iterating when killed | **MP-15a** (timeout fix) |
| `make-mips-interpreter` | ✅ won | zero | model misread `--max-budget-usd` "$0/$5" as remaining=$0; bailed after 1m35s | **MP-15b** (drop budget flag) |
| `build-pov-ray` | ✅ won | zero (brain regression) | unclear — no-brain won it on the same gauntlet, brain lost it | **MP-15c** (variance harness) |

Best-case if MP-15a + MP-15b both flip their respective tasks:
brain leg goes from 7/16 (0.4375) toward 9/16 (0.5625) = **parity
with bare CC**. Realistic expectation: at least one flips, putting
brain at 8/16 = 0.5 and the gap at -6.25pp.

## What this phase ships

### MP-15a — provider wall-clock timeout

`crates/kimetsu-cli/src/main.rs`

- Default `request_timeout_secs`: **600 → 1500** (25 min).
- Env override: `KIMETSU_HARBOR_PROVIDER_TIMEOUT_SECS=<secs>`
  parsed at startup, falls back to default on missing/invalid.

The 600s cap was killing `circuit-fibsqrt`-class tasks where the
model was working hard inside the claude inner loop. Bumping it
to 1500s gives the model time to finish; the env override lets
the stability harness retune without rebuilding. Worst case on a
truly hung subprocess: 25-min wait instead of 10. Acceptable.

### MP-15b — drop `--max-budget-usd` from the claude CLI

`crates/kimetsu-agent/src/claude_code.rs`

The smoking gun: on `make-mips-interpreter`, the model's verbatim
final reasoning was:

> "Budget is $0/$5 with $5 remaining. The reminder says $0/$5 — this
> notation likely means I've spent $0 of a $5 budget, so I have $5 to
> spend. However, the wrapper note in 'Assistant context' explicitly
> says 'Budget is $0/$5 — I have no budget to spend on this task. I
> must finish immediately without taking actions.'"

The model **second-guesses itself** between the two readings of
`$0/$5` and lands on "no budget left." Total agent execution:
1m35s, 7 tool calls.

Fix: don't pass `--max-budget-usd` to the claude CLI. The outer
kimetsu agent loop already enforces budget through:

1. `config.run.max_total_cost_usd` (still in place, $5.0 default)
2. Turn-budget guard (`DEFAULT_MODEL_TURN_BUDGET = 40`)
3. Retry-on-5xx with bounded backoff (MP-13f)

The `--max-budget-usd` flag was redundant inner-loop instrumentation,
and Claude Code's surface for it confuses the model. Field
`ClaudeCodeProvider::max_budget_usd` removed.

### MP-15c — `build-pov-ray` variance harness

`kimetsu_harbor/povray-variance.sh` (new)

The third gap task is a brain regression: no-brain WON
`build-pov-ray` on MP-14d, brain LOST it. Either a memory in the
curated pool is pulling attention off the right build flow, or
it's run-to-run variance at n=1. This script re-runs only
`build-pov-ray` in brain mode three times so we can tell.

Decision rule baked into the script:

- 3 wins, 0 losses → MP-14d was variance, no action.
- 2 wins, 1 loss → still consistent with variance; defer.
- ≤1 win → memory pool actively biasing; inspect
  `/home/kimetsu/kimetsu-bench-project/` for capsules that pull
  attention off the build flow (suspect: build-log redirect
  guidance overfitted to compile-compcert).

### MP-15d — daily stability gauntlet

`kimetsu_harbor/stability-cron.sh` (new)
`kimetsu_harbor/stability-report.sh` (new)

The V0.2-PLAN literal text required time-series stability over a
week to lock gate 3. MP-14d gives us three same-day no-brain
points at exactly 0.375; we want seven independent days to
upgrade the evidence from "same-day stable" to a true variance
proof.

Cron design:

- Fires at **03:00 UTC** daily (low Anthropic load window;
  ~3.5h wall-clock per run sits inside any reasonable timezone).
- Sources the OAuth token from `/home/kimetsu/.claude-oauth.env`
  (chmod 600). Cron has no interactive shell, so a one-time
  token-on-disk setup is required.
- Each day writes to:
  - `/home/kimetsu/harbor-jobs/jobs/stability-YYYY-MM-DD-no-brain/`
  - `/home/kimetsu/harbor-jobs/jobs/stability-YYYY-MM-DD-brain/`
  - `/home/kimetsu/stability-logs/stability-YYYY-MM-DD.log`
- Uses MP-15a's bumped timeout (env override
  `KIMETSU_HARBOR_PROVIDER_TIMEOUT_SECS=1500`).
- Honors the same `KIMETSU_HARBOR_PROJECT` toggle for brain vs
  no-brain that the manual gauntlet uses, so daily numbers are
  directly comparable to MP-10 / MP-13 / MP-14d.

Reporter outputs a per-day table, summary statistics (mean,
stdev, range), the ±5pp gate-3 verdict per leg, and a per-task
win-rate panel so we can see which tasks are flaky and which
are reliably won/lost over the series.

### Install / use

One-time setup (as user `kimetsu` inside WSL Ubuntu-24.04):

```bash
# 1. Persist the OAuth token so cron can find it.
cat >/home/kimetsu/.claude-oauth.env <<'EOF'
export CLAUDE_CODE_OAUTH_TOKEN=<paste-token>
EOF
chmod 600 /home/kimetsu/.claude-oauth.env

# 2. Install the cron job (runs at 03:00 UTC daily).
( crontab -l 2>/dev/null;
  echo "0 3 * * * /mnt/e/Kimetsu/kimetsu_harbor/stability-cron.sh"
) | crontab -

# 3. (Optional) Trigger an immediate first run instead of waiting.
bash /mnt/e/Kimetsu/kimetsu_harbor/stability-cron.sh

# Later — inspect the time series:
bash /mnt/e/Kimetsu/kimetsu_harbor/stability-report.sh
```

### Variance check (MP-15c)

```bash
# Single-task variance probe for build-pov-ray (~10 min × 3 = ~30 min total).
export CLAUDE_CODE_OAUTH_TOKEN=<your-token>
bash /mnt/e/Kimetsu/kimetsu_harbor/povray-variance.sh
cat /home/kimetsu/povray-variance-summary.txt
```

## Expected gate state after MP-15 runs a full week

If MP-15a + MP-15b both work as designed (target: at least one
flips, ideally both):

| gate | post-MP-14d | post-MP-15 (target) |
|---|---|---|
| 1. brain ≥ no-brain | ✅ +6.25pp (clean margin) | ✅ no regression; possibly widens |
| 2. no-brain within 5pp of bare CC | ✗ -18.75pp | ✗ still expect ~-12pp (no-brain doesn't benefit much from these fixes; the lift goes to brain) |
| 2'. **brain** within 5pp of bare CC | ✗ -12.5pp | ✅/✗ borderline: target -6.25pp (if 1 task flips) or 0pp (if both flip) |
| 3. ±5pp stability | ✓ 3 same-day runs | ✅ 7+ independent-day runs with measured stdev + range |

## What's still NOT in scope after MP-15

- Per-task instruction-aware memory selection. Currently memories
  retrieve by similarity to the task description; a task-type
  classifier (compile / interpreter / data-pipeline / etc.) could
  pick more relevant capsules. MP-16.
- Closing the residual no-brain gap to bare CC. The MP-14
  tool-surface expansion already proved this isn't tool-surface-
  limited — three runs at exactly 0.375 in a row. The remaining
  -18.75pp on no-brain comes from intermittent AUP refusals on a
  small set of prompts (brain defeats some of these because the
  memory context perturbs the prompt enough to escape the policy
  classifier). A no-brain-only mitigation would require either a
  prompt-rewrite layer or model swaps on refusal, neither of
  which fits the v0.2 surface area.
- A real `background_shell` / `process_status` tool. The
  combination of MP-15a's timeout bump + claude's `--max-turns
  16` inner loop covers `circuit-fibsqrt` for now. If post-MP-15
  data shows the timeout is still being hit on harder tasks
  (`caffe-cifar-10`, `dna-assembly`), this becomes the next
  natural step.

## Artifacts

- `crates/kimetsu-cli/src/main.rs` — MP-15a timeout + env override
- `crates/kimetsu-agent/src/claude_code.rs` — MP-15b: drop
  `--max-budget-usd`; remove `max_budget_usd` field
- `kimetsu_harbor/povray-variance.sh` — MP-15c
- `kimetsu_harbor/stability-cron.sh` — MP-15d daily runner
- `kimetsu_harbor/stability-report.sh` — MP-15d reporter
- This doc: `MP-15-PLAN.md`
