# Kimetsu Algorithm

How Kimetsu turns memory into measurable savings: every cited memory is
credited with the tokens it saved, every injection is charged for the tokens
it cost, and `kimetsu brain roi` reports the net. This page documents the
exact accounting so the ledger can be audited rather than trusted.

> **Source of truth for the per-kind constants:**
> `crates/kimetsu-brain/src/roi.rs`, `SAVED_TOKENS_PER_CITATION`

---

## Formula

```
estimated_saved_tokens = Σ (citations_of_kind × saved_tokens_per_kind)
net_tokens             = estimated_saved_tokens − injected_tokens
net_usd                = net_tokens / 1_000_000 × price_per_mtok
```

- **`citations`**: rows in `memory_citations` for the selected window (attributed
  to runs via `runs.started_at` or to the hook sentinel via `memory_citations.cited_at`).
- **`injected_tokens`**: sum of the `used_tokens` field across `context.injected`
  events in the window.  This is the actual token cost the brain added to each
  prompt: the "spent" side of the ledger.
- **`price_per_mtok`**: resolved from `[model] price_per_mtok` in `project.toml`
  (user override), falling back to the built-in approximate table in `roi.rs`.
  Unknown models produce `usd: null` in the JSON output rather than a
  potentially-wrong number.

---

## Per-kind constants

| Kind              | Saved tokens / citation | Reasoning |
|-------------------|------------------------:|-----------|
| `failure_pattern` |                   1 500 | Avoids the "try → fail → diagnose → fix" loop. Estimated at ~3 tool calls × ~500 tokens/call. |
| `fact`            |                     500 | Avoids a docs lookup or user question. ~1 exchange. |
| `command`         |                     400 | Avoids a `--help` trial or web search. ~1-2 tool calls. |
| `convention`      |                     300 | Avoids a code-search to find the project pattern. ~1-2 searches. |
| `preference`      |                     200 | Avoids one clarifying question. ~1 exchange. |

**These are deliberate under-estimates.**  The goal is for every "net positive"
result to be _credible_, not just flattering.  A user who sees a positive
number should be able to believe it.

---

## Why under-claiming is deliberate

Kimetsu's ROI story is built on honesty:

1. **Net CAN be negative**, and is shown as such.  If the brain injected more
   tokens than it saved (e.g. every retrieved memory was a silent passenger), the
   ledger says so.
2. **Constants are calibrated low**: the actual avoided cost of re-discovering
   a `failure_pattern` is often much higher (the model might run the same broken
   command five times before giving up).  We use a conservative floor.
3. **Citations are sparse in MCP hosts**: Claude Code currently has no
   `kimetsu_brain_cite` tool, so the `memory_citations` table is populated only
   when the pipeline (`run coding`) is in use.  The ledger will under-count
   savings in pure MCP-host sessions until the cite tool ships.  That's fine:
   under-counting keeps the results trustworthy.

---

## Sanity anchor: Terminal-Bench calibration evidence

The 16-task slice recorded in internal bench runs shows:

| Condition | Cost/win |
|-----------|----------:|
| Without brain context | $2.47 |
| With brain context    | $0.19 |

This ~13× difference is the _observed_ cost difference, not a model.

> **v2.6.0 re-measurement did not reproduce this.** A 12-task slice stratified
> across all nine families of `prog-families-v1`, run on Claude Opus 5 (24
> trials, no grading or quota errors), came out:
>
> | | wins | cost | cost/win | input tokens |
> |---|---|---|---|---|
> | `claude+km` | 8/12 | $82.19 | **$10.27** | 13.9M |
> | `claude` | 9/12 | $67.32 | **$7.48** | 11.4M |
>
> Read this carefully in both directions. The **win-rate** difference is one
> task and is not a result: the discordant pairs are 1 vs 2, a single flip
> reverses the ranking, and a repeat of `cancel-async-tasks` in this same
> session scored differently between runs. "No detectable difference in win
> rate" is what n=12 supports — not that Kimetsu is worse.
>
> The **cost** difference is more solid, because token counts are measured
> rather than sampled: Kimetsu used 22% more total tokens and 37% more per win.
> That direction is the opposite of the table above.
>
> Three things separate the two measurements, and none of them rescues the 13×
> figure: different model (Opus 5 vs Sonnet 3.5), different and unenumerated
> task slice, and cost here is reconstructed from recorded token counts at
> published pricing because Harbor reports a null `cost_usd` under
> subscription auth.
>
> The honest reading is that **13× is not a claim this project can currently
> support**. It is retained above as the historical measurement it was, with
> this caveat attached, until a matched re-run replaces it. It should not be
> quoted without this paragraph.
>
> There is one structural point worth stating, and it cuts both ways:
> Terminal-Bench gives each task a fresh container and the tasks are unrelated,
> so there is almost no prior-session knowledge for a cross-session memory
> system to retrieve — the weakest possible setting for Kimetsu's actual
> thesis. That is a fair explanation of the null. It is *not* a defence of the
> 13× number, because that number was measured on the same instrument.

The per-kind constants were back-derived from this data: if a typical winning
run uses ~2 citations of mixed kinds, the implied saving is roughly
($2.47 − $0.19) / 2 ≈ $1.14/citation at Claude Sonnet 3.5 pricing
(≈$3/MTok → ~380k tokens / citation).  Our constants sum to far less than
that for a 2-citation session, confirming that we are well inside the
conservative zone.

---

## Limitations

1. **Citations are sparse in MCP-host sessions.**  Until `kimetsu_brain_cite` is
   deployed, the ledger only counts citations generated by the autonomous
   pipeline.  The `net_tokens` figure may appear falsely negative for users
   who work exclusively via the Claude Code plugin.
2. **Counterfactuals are estimates, not measurements.**  We cannot observe what
   the model *would* have done without the brain context.  The constants are
   conservative estimates based on tool-call patterns, not A/B experiments.
3. **Price table is approximate.**  The built-in $/MTok figures are rounded
   retail/API-key prices as of June 2026.  Set `[model] price_per_mtok` in
   `project.toml` for accurate billing data.
4. **Injection cost uses input-token pricing.**  The model also produces output
   tokens in response to the injected context; those are not counted here
   (they depend on the model's response length, which varies).

---

## Config reference

```toml
# project.toml: override the built-in price table for this project
[model]
model = "claude-sonnet-5"
price_per_mtok = 3.0   # optional; defaults to built-in table
```

```
# CLI
kimetsu brain roi              # last 30 days (default)
kimetsu brain roi --window 7d  # last 7 days
kimetsu brain roi --window all # all time
kimetsu brain roi --json       # machine-readable RoiReport
```
