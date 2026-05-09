# SWE-bench Integration Plan

Status: **scaffolding only in v0.1**. The CLI command, JSONL parser, and pipeline wiring are in. The repo-prep, gold-patch scoring, and harness comparator pieces are deferred to v0.2.

## What v0.1 ships

- `kimetsu-agent::swe_bench` module: JSONL parser for the public SWE-bench instance schema (Lite + Pro fields tolerated via `#[serde(default)]`), and a `run_swe_bench` function that drives `pipeline::run_coding` against a caller-supplied repo.
- `kimetsu bench swe` CLI command:
  ```
  kimetsu bench swe \
      --tasks path/to/instances.jsonl \
      --repo /path/to/already/prepared/repo \
      [--instance-id django__django-12345] \
      [--dry-run] [--no-broker] [--limit 5]
  ```
- Tests proving the parser handles minimal records and that the brief sent to the model includes hints, fail_to_pass cases, and the problem statement.

What v0.1 does **not** do:

- Clone the upstream repo at `base_commit`
- Apply `test_patch` before invoking the pipeline
- Score the resulting patch against `patch` (the gold patch)
- Run anything in parallel
- Distinguish SWE-bench Lite, Verified, and Pro at the runtime layer

## Why deferred

Each missing piece is a real engineering surface:

- **Cloning**: needs disk-space management, GitHub auth, retry on rate limits, optional Git LFS handling. Doable but a 2–3 day chunk on its own.
- **`test_patch` application**: SWE-bench `test_patch` is a unified diff against the upstream repo. Applying it requires either `git apply --check && git apply` or a Rust unified-diff applier; v0.1 explicitly chose whole-file replacement only (`apply_patch` tool ships no fuzzy applier). Adding one for SWE-bench prep would partially undo that decision.
- **Scoring**: SWE-bench scoring runs the upstream test suite at the original `base_commit` plus any patch the agent produced and compares against `fail_to_pass` / `pass_to_pass` test sets. The accepted way is `swebench` Python harness; replicating that in-tree is duplicative.

The pragmatic v0.2 plan treats Kimetsu as the *agent under test* and SWE-bench's own harness as the runner.

## v0.2 plan: agent-under-test mode

```
swebench harness
  → for each instance:
      ├─ checkout upstream repo @ base_commit
      ├─ apply test_patch
      ├─ invoke `kimetsu bench swe --tasks <one-instance.jsonl> --repo <checkout>`
      ├─ collect git diff after kimetsu finishes
      ├─ score diff against patch / fail_to_pass / pass_to_pass
      └─ persist score
```

This approach lets us inherit the upstream scoring (which is the part we'd otherwise be reimplementing) and keeps Kimetsu's job tight: "given a repo and a task, produce a patch that passes the verification commands."

What v0.2 needs to add inside Kimetsu:

1. **Patch capture mode**: at the end of a run, emit `git diff` between `base_commit` and the working tree to a known artifact path so the SWE-bench harness can read it without scraping events.
2. **Verification autodetect for non-Cargo repos**: today's verification command detection covers `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`. SWE-bench is python-heavy; ensure pytest paths resolve correctly inside the upstream repo's pyenv.
3. **Optional non-modifying `test_patch` injection**: Kimetsu treats `test_patch` as a constraint that the failing tests must pass; surfacing it to the agent as part of the prompt (already done via `format_task_brief`) is one path. A future improvement could mark those tests as the verification gate's source of truth.
4. **Cost throttling per instance**: SWE-bench Lite has 300 instances; running each at $0.20 = $60. Verified is 500 instances. Pro is even bigger. The bench needs an explicit per-run budget the harness can pass via env or flag.
5. **Stable timeout policy**: SWE-bench instances have median wall-clock budgets; we should add a `--max-wallclock-secs` flag and honor it inside the agent loop.

## v0.3 vision: brain-on vs brain-off on a real benchmark

Once v0.2 lands and Kimetsu can be scored by the upstream harness, we run the same instance set in three modes — `brain_off` / `brain_on_cold` / `brain_on_warm` — and compare resolved% per mode. The MVP.md falsifiable claim becomes:

> warm follow-up tasks resolve at least 20% more SWE-bench Lite instances than cold,
> or use ≥20% fewer total tool calls at parity success.

That comparison is the real falsifiable test of the brain's value, beyond the synthetic 16-task internal bench.

## Open questions

1. How do we seed memory for SWE-bench? The internal bench uses a hand-curated `warm_memory` per task. For SWE-bench we'd either:
   - Accept that warm = cold (no seeded memory), and only measure broker-on vs broker-off; or
   - Pre-seed the brain with memories derived from a *first pass* over the same instance set ("having already seen this codebase, rerun"). This is closer to MVP.md's intent.
2. Does `--no-broker` map cleanly to SWE-bench? The broker still ingests the upstream repo on a brain_off run, so brain_off measures "model with raw repo access but no Kimetsu memory or capsule retrieval." That's a meaningful baseline.
3. Do we keep failing-test fingerprint stop active? Pytest stack traces are noisy; the existing fingerprint normalizer (paths stripped, digits collapsed, ANSI removed) should hold but needs validation against real pytest output.

## Smoke instructions for the v0.1 scaffolding

To exercise the scaffolding today against a single SWE-bench instance you've already prepped on disk:

```
# 1. Pull a SWE-bench Lite instance JSON line into a one-line jsonl file.
echo '{"instance_id":"...","repo":"...","base_commit":"...","problem_statement":"..."}' > one.jsonl

# 2. Manually clone the upstream repo, checkout base_commit, apply test_patch.
#    (v0.1 does not do this for you.)

# 3. Run kimetsu against it.
kimetsu bench swe --tasks one.jsonl --repo /path/to/prepped/repo --dry-run

# 4. Inspect the trace + final report under .kimetsu/runs/<run_id>/.
```

This won't produce a SWE-bench score; it produces a Kimetsu trace and PatchPlan you can read by hand. That's the point of the v0.1 scaffolding — keep the integration honest about where the work actually is.
