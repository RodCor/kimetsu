# Kimetsu Ã— Harbor / Terminal-Bench adapter

This directory contains the Python wrapper that lets Harbor (the official
Terminal-Bench harness) drive Kimetsu as an external agent. It is benchmark
glue only. The user-facing Kimetsu CLI is the Rust `kimetsu` binary; Harbor
uses the separate Rust `kimetsu-harbor-agent` binary over a line-oriented
JSON-RPC protocol.

This is MP-7b in `docs/archive/V0.2-PLAN.md`. The protocol spec lives in
`crates/kimetsu-harbor-rs/src/protocol.rs`.

## Pieces

| file | purpose |
|------|---------|
| `kimetsu_agent.py` | The `KimetsuAgent(BaseAgent)` class Harbor instantiates. ~250 lines. No deps beyond stdlib + optional Harbor BaseAgent. |
| `smoke_test.py` | Drives the adapter against a fake environment so you can verify the round-trip without Harbor / Docker installed. |
| `__init__.py` | Package marker; exposes `__version__`. |

## Prerequisites

1. **Harbor adapter binary built**: `cargo build -p kimetsu-harbor-rs --release` (or
   `--debug` is fine for smoke testing). The adapter auto-detects
   `kimetsu-harbor-agent` / `kimetsu-harbor-agent.exe` on PATH, then walks
   `target/release/kimetsu-harbor-agent[.exe]` and
   `target/debug/kimetsu-harbor-agent[.exe]` relative to CWD. Set
   `KIMETSU_HARBOR_BIN=/abs/path/to/kimetsu-harbor-agent` to override.
2. **Python 3.10+** (uses PEP 604 `str | None` union syntax).
3. **Harbor CLI**: the package on PyPI is named `harbor`
   (not `harbor-framework`):

   ```bash
   pip install harbor           # installs harbor.exe / hb.exe / hr.exe
   # or, per Harbor's own docs:
   uv tool install harbor
   harbor --version             # 0.6.x at the time of writing
   ```

   On Windows the binaries land in
   `C:\Users\<you>\AppData\Roaming\Python\Python313\Scripts`; add that
   directory to `PATH` if pip warns it isn't on PATH.
4. **An environment to run the task in.** Harbor supports many:

   | `-e` flag | what it is | needs |
   |-----------|------------|-------|
   | `docker`   | local Docker | Docker Desktop running on the host |
   | `daytona`  | cloud sandbox | `DAYTONA_API_KEY` |
   | `e2b`      | cloud sandbox | `E2B_API_KEY` |
   | `modal`    | cloud sandbox | `MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET` |
   | `runloop`  | cloud sandbox | `RUNLOOP_API_KEY` |

   For v0.2 the official Terminal-Bench leaderboard runs use either
   `docker` (local) or `daytona` (per the Harbor docs). Pick whichever
   matches what's set up on your machine.
5. **API credentials for the model**: `CLAUDE_CODE_OAUTH_TOKEN` for
   `kimetsu-harbor-agent` model runs.
   Pass via env or `--env-file path/to/.env` on `harbor run`.

## Smoke test (no Harbor needed)

```bash
# from the repo root
cargo build -p kimetsu-harbor-rs
python kimetsu_harbor/smoke_test.py
```

Expected output:

```
[smoke] using harbor agent binary: .../target/debug/kimetsu-harbor-agent(.exe)
[smoke] context keys: ['summary', 'tool_calls', 'kimetsu_context', 'protocol_version']
[smoke] summary: 'MP-7c multi-step stub for `harbor adapter smoke test` completed; protocol=0.1'
[smoke] tool_calls: [{'program': 'pwd', ...}, {'program': 'echo', ...}]
[smoke] environment received 2 command(s)
[smoke] OK
```

The smoke test stubs Harbor's environment with a `subprocess.run`-based
mock so the `tool.exec` round-trip actually runs against the host
shell. If the smoke test passes, the protocol bridge is correct and you
can move on to a real Harbor run.

## Real Terminal-Bench run

Once Harbor + an environment are set up:

```bash
# 1. Sanity: oracle run confirms Harbor + environment are wired up.
harbor run --dataset terminal-bench/terminal-bench-2 -a oracle -n 4

# 2. Kimetsu's three-mode gauntlet (per docs/archive/V0.2-PLAN.md MP-8).
#
# a) Bare Claude Code baseline (no kimetsu wrapper).
harbor run --dataset terminal-bench/terminal-bench-2 \
  -a claude-code -m claude-haiku-4-5 -n 4

# b) Kimetsu with no brain (broker disabled). MP-7d real model loop;
#    the model only sees shell_command and works the task without any
#    broker grounding or memory injection.
PYTHONPATH="$(pwd)" \
KIMETSU_HARBOR_BIN="$(pwd)/target/release/kimetsu-harbor-agent" \
  harbor run --dataset terminal-bench/terminal-bench-2 \
    --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
    -n 4

# c) Kimetsu with brain + curated memories. Same invocation; the
#    broker uses whatever was curated via `kimetsu brain memory
#    review` / `memory top` / `memory prune` on the host.
PYTHONPATH="$(pwd)" \
KIMETSU_HARBOR_BIN="$(pwd)/target/release/kimetsu-harbor-agent" \
KIMETSU_HARBOR_PROJECT="$(pwd)" \
  harbor run --dataset terminal-bench/terminal-bench-2 \
    --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
    -n 4
```

On Windows / PowerShell:

```powershell
$env:PYTHONPATH = (Get-Location).Path
$env:KIMETSU_HARBOR_BIN = "$pwd\target\release\kimetsu-harbor-agent.exe"
harbor run `
  --dataset terminal-bench/terminal-bench-2 `
  --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent `
  -n 4
```

Per the v0.2 ship gate (docs/archive/V0.2-PLAN.md MP-8): three runs per mode within
Â±5pp over a 1-week window. Stability matters more than peak accuracy.

## Codex + Kimetsu MCP run

Harbor 0.7.x includes a built-in `codex` agent with `--mcp-config` and
`--skill` support. On the WSL setup from `SETUP-WSL.md`, run Codex inside
Terminal-Bench while exposing Kimetsu as an MCP sidecar:

```powershell
.\kimetsu_harbor\run-codex-kimetsu-wsl.ps1 -TaskLimit 16 -Concurrency 2 -Model gpt-5.5
```

The script:

- rebuilds the Linux `target/release/kimetsu` binary if it is missing or stale;
- builds a static `target/x86_64-unknown-linux-musl/release/kimetsu` binary for
  benchmark containers, so the MCP helper is not tied to the WSL host glibc;
- prepares the Kimetsu brain repo index for the WSL path root when missing
  (`KIMETSU_BRAIN_PREP=auto`, set `0` to skip or `1` to force re-ingest);
- bind-mounts `/mnt/e/Kimetsu` into each benchmark container, read-only by default;
- generates a Harbor MCP config equivalent to `codex-kimetsu-mcp.wsl.json`;
- points Codex at `kimetsu_harbor/kimetsu-mcp-stdio.sh`, a no-argument wrapper
  that avoids Harbor's Codex adapter flattening MCP command arguments into one
  non-executable command string. The wrapper prefers the static musl binary and
  falls back to `target/release/kimetsu`;
- exposes `kimetsu_harbor/kimetsu-brain-context.sh` as a fallback helper for
  harnesses that receive the MCP config but do not surface external MCP tools to
  the model. Set `KIMETSU_TOOL_NAME=kimetsu_benchmark_context` to request the
  benchmark playbook tool instead of the generic brain context tool;
- probes `tools/list` inside `debian:bookworm` before launching a model run and
  fails fast if `kimetsu_brain_context` or `kimetsu_benchmark_context` is not
  visible;
- injects `.codex/skills/kimetsu-bridge` as a Harbor skill;
- appends a Kimetsu mode instruction with `--extra-instruction-path`;
- in `required` mode, uses `kimetsu_harbor.codex_kimetsu_agent:CodexKimetsuRequired`
  to fetch `kimetsu_benchmark_context` before Codex starts and prepend the
  returned Kimetsu Benchmark Playbook to the task prompt;
- uploads the host Codex auth JSON from `/mnt/c/Users/rodri/.codex/auth.json`.

Three Kimetsu MCP modes are supported:

| mode | behavior |
|------|----------|
| `optional` | Kimetsu is available as memory/brain context. For Terminal-Bench, the agent should call `kimetsu_benchmark_context`; for other work it may call `kimetsu_brain_context` or related tools when useful. |
| `required` | Kimetsu brain usage is enforced outside the model loop. Harbor calls the mounted MCP stdio helper before Codex starts, writes `kimetsu-brain-*` artifacts into the trial logs, and prepends a compact benchmark playbook to the task. Codex still receives the MCP config and bridge skill for follow-up calls. |
| `none` | Baseline Codex run. The runner does not pass a Kimetsu MCP config, bridge skill, extra instruction, or Kimetsu repo mount. |

Benchmark brain warmth is tracked separately from the MCP availability mode:

| warm policy | behavior |
|-------------|----------|
| `cold_brain` | Broker/repo/prior-run grounding is allowed, but accepted memory capsules are excluded from the playbook. This mirrors the older `brain_on_cold` research condition. |
| `reactive_warm` | Kimetsu memory is available when the model or harness asks for it, but task-specific benchmark memory is not required up front. This maps to optional/reactive usage. |
| `full_warm` | The playbook is fetched before Codex starts and may include task-specific benchmark memories. This is the required-mode prefetch condition. |

Required Codex prefetch defaults to `KIMETSU_BRAIN_WARM_POLICY=full_warm`.
Set `KIMETSU_BRAIN_WARM_POLICY=cold_brain` to measure a brain-on cold run, or
`reactive_warm` when comparing against the reactive-warm Claude Code research.

The mode files live in `kimetsu_harbor/kimetsu-mcp-optional.md` and
`kimetsu_harbor/kimetsu-mcp-required.md`. They are plain Harbor extra
instructions, so they can be reused with any MCP-capable Harbor agent such as
Codex or Claude Code:

```bash
harbor run -d terminal-bench/terminal-bench-2 -a codex \
  --mcp-config /path/to/kimetsu-mcp.json \
  --extra-instruction-path kimetsu_harbor/kimetsu-mcp-required.md
```

Codex should treat Kimetsu's brain as the primary value of this MCP sidecar.
For Terminal-Bench, call `kimetsu_benchmark_context` with the task text and
dataset, then use the returned `playbook_markdown` as working context before
solving. For non-benchmark work, call `kimetsu_brain_context` early with the
task text. After a benchmark attempt, call `kimetsu_benchmark_record_outcome`
with pass/fail/error status, key commands, pitfalls, and verification so
future runs retrieve exact episodic evidence. When the attempt reveals a
transferable tactic or warning, also pass `generalized_memory` with
`memory_role=semantic_operator` or `anti_pattern`, plus optional `task_family`,
`applies_to`, `does_not_apply_to`, and review rationale fields. Kimetsu keeps
that generalized memory pending until review, so the durable brain improves
without overfitting to one Terminal-Bench slug. Use `kimetsu_brain_status`,
`kimetsu_brain_memory_top`, and the proposal accept/reject/invalidate tools
when inspecting or curating the memory pool. Bridge tools such as
`kimetsu_skills_search` remain available for portable skill lookup and setup.

Required mode defaults to `KIMETSU_CODEX_ENFORCE_BRAIN=auto`, which enables
the prefetch wrapper only for `KIMETSU_MCP_MODE=required`. Set
`KIMETSU_CODEX_ENFORCE_BRAIN=0` to fall back to prompt-only required mode, or
`KIMETSU_CODEX_ENFORCE_BRAIN=1` to force prefetch for any Kimetsu-enabled mode.
Required mode fails fast if the brain call itself fails. Set
`KIMETSU_REQUIRE_NONEMPTY_BRAIN=1` only when you want zero retrieved capsules to
fail the task before Codex starts.
Set `KIMETSU_REQUIRE_BENCHMARK_MEMORY=1` when you want strict benchmark mode to
fail unless `kimetsu_benchmark_context` retrieves at least one exact-slug
episodic memory or generalized semantic/anti-pattern benchmark memory.

Useful overrides:

```bash
TASK_LIMIT=1 N_CONCURRENT=1 JOB_NAME=codex-kimetsu-smoke KIMETSU_MCP_MODE=required \
  bash /mnt/e/Kimetsu/kimetsu_harbor/run-codex-kimetsu-bench.sh
```

The static container binary build uses the Rust `x86_64-unknown-linux-musl`
target. If WSL does not already have the musl C toolchain, install it once with
`apt-get update && apt-get install -y musl-tools`, or set
`KIMETSU_CONTAINER_BINARY_BUILD=0` only after building the binary yourself.

Preflight only, with no model call:

```powershell
.\kimetsu_harbor\run-codex-kimetsu-wsl.ps1 -Preflight
```

Required-mode one-task smoke from PowerShell:

```powershell
.\kimetsu_harbor\run-codex-kimetsu-wsl.ps1 -TaskLimit 1 -Concurrency 1 -KimetsuMode required -JobName codex-kimetsu-required-smoke
```

Set `KIMETSU_MOUNT_READ_ONLY=0` only if the benchmark intentionally needs
Codex to call Kimetsu bridge tools that write to the mounted Kimetsu workspace.

## Adapter status

`kimetsu-harbor-agent` owns the benchmark-only JSON-RPC entry point.
The regular `kimetsu` binary does not expose Harbor flags.

## Compatibility notes

Harbor's `environment.exec` return-value shape is not exhaustively
documented at the time of writing. The adapter's `_normalize_exec_result`
function probes four common shapes:

1. tuple `(stdout, stderr, exit_code)` or `(exit_code, stdout, stderr)`
2. mapping / dict with `exit_code` (or `returncode`) + `stdout` + `stderr`
3. object with `.exit_code`/`.stdout`/`.stderr` attributes (typed dataclass)
4. instance of the adapter's own `ExecResult` dataclass

Unknown shapes return `exit_code=255` with a descriptive stderr so the
model sees a clear failure instead of silently succeeding. If Harbor's
contract changes, update `_normalize_exec_result` and the smoke test's
`FakeEnvironment` accordingly.

## Troubleshooting

- **`could not locate kimetsu-harbor-agent`**: Run `cargo build
  -p kimetsu-harbor-rs --release` and either drop the binary into a PATH
  directory or set `KIMETSU_HARBOR_BIN` to its absolute path.
- **`kimetsu-harbor-agent exited before emitting agent.done`**: The Rust
  subprocess died. Inspect its stderr â€” the harbor-mode session never
  writes there for normal control flow, so any output is an error.
  Common cause: malformed JSON on the adapter's reply (check that
  PowerShell isn't inserting a UTF-16 BOM if you're scripting stdin).
- **`environment object has no exec`**: You're driving the adapter with
  something that doesn't quack like Harbor's environment. The smoke
  test demonstrates the minimum surface area.
- **`GLIBC_2.39 not found` from `kimetsu-brain-context.sh`**: The helper is
  running the host glibc binary inside an older Terminal-Bench container. Re-run
  the benchmark wrapper so it builds
  `target/x86_64-unknown-linux-musl/release/kimetsu`, then confirm preflight
  lists `kimetsu_brain_context`.
