# Kimetsu Ã— Harbor / Terminal-Bench adapter

This directory contains the Python wrapper that lets Harbor (the official
Terminal-Bench harness) drive Kimetsu as an external agent. The Rust
harness itself lives in `crates/`; everything in this folder is the
glue that bridges Harbor's Python API to `kimetsu agent --harbor-mode`
over a line-oriented JSON-RPC protocol.

This is MP-7b in `V0.2-PLAN.md`. The protocol spec lives at the top of
`crates/kimetsu-agent/src/harbor.rs`.

## Pieces

| file | purpose |
|------|---------|
| `kimetsu_agent.py` | The `KimetsuAgent(BaseAgent)` class Harbor instantiates. ~250 lines. No deps beyond stdlib + optional Harbor BaseAgent. |
| `smoke_test.py` | Drives the adapter against a fake environment so you can verify the round-trip without Harbor / Docker installed. |
| `__init__.py` | Package marker; exposes `__version__`. |

## Prerequisites

1. **Rust binary built**: `cargo build -p kimetsu-cli --release` (or
   `--debug` is fine for smoke testing). The adapter auto-detects
   `kimetsu` / `kimetsu.exe` on PATH, then walks
   `target/release/kimetsu[.exe]` and `target/debug/kimetsu[.exe]`
   relative to CWD. Set `KIMETSU_BIN=/abs/path/to/kimetsu` to override.
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
   kimetsu's real model loop (default in `kimetsu agent --harbor-mode`).
   Pass via env or `--env-file path/to/.env` on `harbor run`.

## Smoke test (no Harbor needed)

```bash
# from the repo root
cargo build -p kimetsu-cli
python kimetsu_harbor/smoke_test.py
```

Expected output:

```
[smoke] using kimetsu binary: .../target/debug/kimetsu(.exe)
[smoke] context keys: ['summary', 'tool_calls', 'kimetsu_context', 'protocol_version']
[smoke] summary: 'stub agent for task `harbor adapter smoke test` completed; protocol=0.1'
[smoke] tool_calls: [{'program': 'echo', 'args': ['...'], 'exit_code': 0}]
[smoke] environment received 1 command(s)
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

# 2. Kimetsu's three-mode gauntlet (per V0.2-PLAN.md MP-8).
#
# a) Bare Claude Code baseline (no kimetsu wrapper).
harbor run --dataset terminal-bench/terminal-bench-2 \
  -a claude-code -m claude-haiku-4-5 -n 4

# b) Kimetsu with no brain (broker disabled). MP-7d real model loop;
#    the model only sees shell_command and works the task without any
#    broker grounding or memory injection.
PYTHONPATH="$(pwd)" \
KIMETSU_BIN="$(pwd)/target/release/kimetsu" \
KIMETSU_DISABLE_BROKER=1 \
  harbor run --dataset terminal-bench/terminal-bench-2 \
    --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
    -n 4

# c) Kimetsu with brain + curated memories. Same invocation; the
#    broker uses whatever was curated via `kimetsu brain memory
#    review` / `memory top` / `memory prune` on the host.
PYTHONPATH="$(pwd)" \
KIMETSU_BIN="$(pwd)/target/release/kimetsu" \
  harbor run --dataset terminal-bench/terminal-bench-2 \
    --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent \
    -n 4
```

On Windows / PowerShell:

```powershell
$env:PYTHONPATH = (Get-Location).Path
$env:KIMETSU_BIN = "$pwd\target\release\kimetsu.exe"
harbor run `
  --dataset terminal-bench/terminal-bench-2 `
  --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent `
  -n 4
```

Per the v0.2 ship gate (V0.2-PLAN.md MP-8): three runs per mode within
Â±5pp over a 1-week window. Stability matters more than peak accuracy.

## MP-7a is a stub agent; MP-7c will plumb the real pipeline

Today the Rust binary runs a one-shot echo through Harbor and emits
`agent.done`. That's deliberate â€” MP-7a exists to lock down the wire
protocol so MP-7b (this adapter) and MP-7c (real pipeline integration)
can land in either order without churn.

When MP-7c lands, the same `kimetsu agent --harbor-mode --task ...`
invocation will route the broker + model + multi-step tool loop through
this adapter unchanged. Nothing on the Python side should need to move.

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

- **`could not locate the kimetsu binary`**: Run `cargo build
  -p kimetsu-cli --release` and either drop the binary into a PATH
  directory or set `KIMETSU_BIN` to its absolute path.
- **`kimetsu agent exited before emitting agent.done`**: The Rust
  subprocess died. Inspect its stderr â€” the harbor-mode session never
  writes there for normal control flow, so any output is an error.
  Common cause: malformed JSON on the adapter's reply (check that
  PowerShell isn't inserting a UTF-16 BOM if you're scripting stdin).
- **`environment object has no exec`**: You're driving the adapter with
  something that doesn't quack like Harbor's environment. The smoke
  test demonstrates the minimum surface area.
