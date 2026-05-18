# MP-13 â€” auto-orient + persistence gate + retry-on-5xx

This phase fixed two distinct issues identified in MP-11/MP-12 results:
specific failure modes the kimetsu wrapper had vs bare Claude Code,
and brittle handling of transient Anthropic API errors.

## What shipped

| change | commit | notes |
|--------|--------|-------|
| MP-13a auto-orient | `07c4693` | Single composite shell call before first model turn dumps pwd/ls/task-instructions/build-sniff into a "Initial workspace state" block prepended to the user message. ~3500 byte cap. |
| MP-13b turn budget 25â†’40 | `07c4693` | Caught compile-compcert / caffe-cifar-10 that ran out of turns in MP-12. |
| MP-13c CC --max-turns 8â†’16 | `07c4693` | Doubled headroom for Claude Code's inner loop. |
| MP-13d persistence gate | `07c4693` | If the model emits a text-only response with < 3 tool_calls, one user-side nudge asks it to actually try; second time finish goes through. |
| MP-13f retry-on-5xx | (this commit) | claude_code provider retries up to 3Ã— with 5s/10s backoff on transient Anthropic errors (api_error_status >= 500 or "Overloaded" text). |
| KimetsuAgentOpts struct | `07c4693` | Cleaner API; tests use `for_tests()` to skip auto-orient + persistence gate. |

## MP-13e results â€” the run that triggered MP-13f

Same 16-task slice on `terminal-bench/terminal-bench-2`, Opus 4.7,
sequential (no-brain â†’ brain), `-n 2 -k 1`.

| run | wins | mean | RuntimeError | AgentTimeoutError | cost |
|-----|-----:|-----:|-------------:|------------------:|-----:|
| MP-13 no-brain | 6 | 0.375 | 3 | 3 | $1.26 |
| MP-13 brain    | **1** | **0.0625** | **14** | 1 | $0.23 |

### no-brain leg: net-flat vs MP-10

Same 6 wins as MP-10 (with `overfull-hbox` swapped for
`log-summary-date-ranges`). MP-13 a/b/c/d **did not increase win
count on the no-brain slice at n=16**. The harness changes are net-
neutral, not a clear win. Cost dropped ~63% (MP-10's $3.45 â†’ $1.26),
meaning the auto-orient + tighter prompts let the model converge in
fewer turns even when it loses.

### brain leg: Anthropic 529 outage, not a kimetsu regression

The 14 RuntimeErrors all carry the same Anthropic API signature:

```
API Error: 529 Overloaded. This is a server-side issue, usually
temporary â€” try again in a moment. If it persists, check status.claude.com.
```

Each failure spent ~210s waiting on the API before the 529 response
came back. The no-brain leg ran first (23:01-23:41 UTC) and saw none
of these; brain ran immediately after (23:41-00:48 UTC) during a
brief Anthropic overload window.

**This is not a kimetsu regression and not a MP-13 regression** â€” it
is a infrastructure-level transient on Anthropic's side that our
provider didn't handle.

## MP-13f: retry-on-5xx in the claude_code provider

Fix: wrap the spawn-and-parse in a retry loop with up to 3 attempts
and 5s / 10s exponential backoff between them. Trigger conditions:

- `is_error: true` AND `api_error_status >= 500` in the parsed JSON
  result, OR
- stdout contains `"Overloaded"` or `"api_error_status":5` as a
  fallback sentinel for cases where the JSON parse fails

Non-transient failures (exit code != 0 with no 5xx signal) fail
immediately as before. Backoff is bounded so a true outage doesn't
balloon trial time past Harbor's per-task timeout.

## Net v0.2 ship-gate state after MP-13

| gate | status | observed |
|------|:------:|----------|
| 1 â€” `kimetsu-brain â‰¥ kimetsu-no-brain` | unresolved | brain leg invalidated by API outage; needs re-run with retry |
| 2 â€” `kimetsu-no-brain` within 5pp of `bare` | âœ— | 0.375 vs 0.5625 = -18.75pp (matches MP-10/MP-11) |
| 3 â€” three runs within Â±5pp | partial | MP-10 + MP-13 no-brain both at 0.375 (perfect tie!), need one more |

**Gate 3 partial pass:** MP-10 no-brain = 0.375 (6/16) and MP-13
no-brain = 0.375 (6/16) on the *same* tasks. Even though the
specific tasks won differ slightly (overfull-hbox vs
log-summary-date-ranges), the overall accuracy is identical across
two independent runs done two days apart. That's a real stability
data point.

## Recommendation

1. Re-run MP-13 brain (only) with the retry-on-5xx provider. Expected
   wall-clock ~70 min, cost ~$1-3 if no API issues.
2. If brain comes in at â‰¥ no-brain, gates 1 + 3 are met. Gate 2
   remains a structural problem (the kimetsu adapter has a different
   capability surface than bare CC).
3. Adopt the cost-based ship framing: kimetsu wraps Opus at ~5% of
   bare CC's cost on Terminal-Bench. That's a real, defensible v0.2
   value prop even if accuracy parity isn't reachable.

## Artifacts

- Job dirs `/home/kimetsu/harbor-jobs/jobs/mp13-no-brain/` and
  `/home/kimetsu/harbor-jobs/jobs/mp13-brain/`
- Per-trial `exception.txt` files document the 14 distinct 529s in
  brain-leg
- Code: harness.rs KimetsuAgentOpts + collect_workspace_orientation
  + persistence gate
- Retry-on-5xx: claude_code.rs complete() loop
