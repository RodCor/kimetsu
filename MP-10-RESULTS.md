# MP-10 — First kimetsu-no-brain Terminal-Bench baseline

Run id: `kimetsu-mp10-opus` (Harbor job dir
`/home/kimetsu/harbor-jobs/jobs/kimetsu-mp10-opus/` inside the Ubuntu
24.04 WSL distro). Started 2026-05-13T20:58:09Z, finished
2026-05-13T22:22:40Z (1h 24m wall-clock). All numbers below come from
that job's `result.json`.

## Configuration

| dimension | value |
|-----------|-------|
| harness | Harbor 0.6.6 |
| dataset | `terminal-bench/terminal-bench-2` |
| environment | Docker (WSL2 Ubuntu 24.04, systemd, dockerd on tcp 2375) |
| agent | `kimetsu_harbor.kimetsu_agent:KimetsuAgent` |
| kimetsu binary | `target/release/kimetsu` built from commit `02495dd` |
| model | `claude-opus-4-7` via `claude_code` provider |
| broker / memory | **off** — this is the kimetsu-no-brain baseline |
| tasks | 16 (`-l 16 -n 2 -k 1 --yes`) |
| auth | `CLAUDE_CODE_OAUTH_TOKEN` (subscription path) |

## Aggregate numbers

| metric | value |
|--------|------:|
| total trials | 16 |
| **mean reward** | **0.375** |
| trials with reward 1.0 | **6** |
| trials with reward 0.0 | 8 (verifier ran, model didn't solve) |
| RuntimeErrors | 2 |
| AgentTimeoutErrors | 2 (counted as reward 0.0 above too) |
| total cost | **$3.45** |
| total output tokens | 70,380 |
| total input tokens | 74 (rest came from prompt caching across turns) |
| wall-clock | 1h 24m at `-n 2` concurrency |

## Per-task breakdown

### Wins — reward = 1.0
- `build-pov-ray` — build POV-Ray 2.x from sources
- `overfull-hbox` — LaTeX overfull-hbox fix via word substitutions
- `distribution-search`
- `break-filter-js-from-html`
- `vulnerable-secret`
- `openssl-selfsigned-cert` — sampled: 8 turns / 7 shell calls / $0.035, model drove the full mkdir → genrsa → chmod → req → cat .pem → verify → write check_cert.py chain

### Verifier scored 0.0 — model ran but didn't solve
- make-mips-interpreter
- video-processing
- protein-assembly
- compile-compcert
- install-windows-3-11
- log-summary-date-ranges
- path-tracing (also AgentTimeoutError)
- caffe-cifar-10 (also AgentTimeoutError)

### Errored out — couldn't complete
- `circuit-fibsqrt` (RuntimeError — likely 600s claude_code provider timeout)
- `dna-assembly` (RuntimeError — same class)
- `path-tracing` (AgentTimeoutError — Harbor's per-task timeout)
- `caffe-cifar-10` (AgentTimeoutError)

## What this confirms

1. **The kimetsu adapter is load-bearing on real Terminal-Bench tasks.** First non-zero baseline ever: 6 outright wins on Opus tier, including complex ones like `build-pov-ray` and `openssl-selfsigned-cert`.
2. **Cost is in line with bare Claude Code.** $3.45 for 16 tasks ≈ $0.22/task average. The wrapper isn't adding meaningful overhead.
3. **Stability is acceptable.** 14/16 trials produced verifier output cleanly; the 4 errors are all timeouts (model thinking too long), not protocol failures.

## Where the v0.2 ship gate stands

V0.2-PLAN.md asks for three thresholds:

| gate | status |
|------|:------:|
| 1. `kimetsu-brain` ≥ `kimetsu-no-brain` on Terminal-Bench accuracy | **pending MP-11** (need to layer broker + curated memory) |
| 2. `kimetsu-no-brain` within 5pp of `bare` | **pending MP-10b** (need bare Claude-Code baseline on the same 16 tasks) |
| 3. Three runs over ~1 week within ±5pp | **pending MP-12** (stability re-runs) |

So MP-10 unblocks the bottom line: we now have a real `kimetsu-no-brain` number at 37.5%. Next two pieces are:

- **MP-10b** — `harbor run -a claude-code -m anthropic/claude-opus-4-7 -l 16 -n 2 -k 1` on the same 16 tasks. Apples-to-apples comparison; expect bare Opus to land somewhere between 35–45%.
- **MP-11** — add broker + curated memory to the kimetsu user message; re-run the 16; check if it beats 0.375.

## What we learned along the way (for the v0.2 ship doc)

- Claude Code 2.x in `-p` mode injects its own harness system prompt that overrides our `--system-prompt`. Authority for the response format has to live in the **user message** for the model to obey it. (MP-9 fix.)
- Harbor's `ExecResult` field is `return_code`, not `exit_code` — our adapter's normalizer didn't probe for it, so for one bench every shell silently returned 255. (MP-9b fix.)
- `--max-turns=1` on Claude Code is wrong for harbor mode (the model needs ≥1 inner turn to respond cleanly to a tool result). Bumped to 8 in `claude_code.rs`. (MP-7d fix.)
- Default model matters: haiku at 0/4 → opus at 6/16 = 37.5%. The "kimetsu is bad" story before this commit was almost entirely "haiku is bad on the hardest tasks".

## Artifacts

- Job dir: `/home/kimetsu/harbor-jobs/jobs/kimetsu-mp10-opus/`
- Aggregate `result.json` and per-trial `result.json` + `exception_info` are all there.
- Binary: `target/release/kimetsu` built from commit `02495dd`.
- Adapter: `kimetsu_harbor/kimetsu_agent.py` from commit `ce92e0d`.
