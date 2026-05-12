# Kimetsu × Harbor / Terminal-Bench adapter

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
3. For real Terminal-Bench runs: **Harbor CLI** and **Docker Desktop**:

   ```bash
   pip install harbor-framework      # or whatever Harbor publishes
   harbor --version                  # verify install
   docker info                       # verify Docker is up
   ```

## Smoke test (no Harbor needed)

```bash
# from the repo root
cargo build -p kimetsu-cli
python harbor/smoke_test.py
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

Once Harbor + Docker are set up:

```bash
# Sanity: oracle run confirms Harbor itself is wired up correctly.
harbor run -d terminal-bench/terminal-bench-2 -a oracle -n 4

# Kimetsu run via this adapter. The --agent-import-path tells Harbor
# to load harbor.kimetsu_agent:KimetsuAgent from PYTHONPATH (set
# PYTHONPATH to the repo root so the import resolves).
PYTHONPATH="$(pwd)" \
KIMETSU_BIN="$(pwd)/target/release/kimetsu" \
  harbor run \
    -d terminal-bench/terminal-bench-2 \
    --agent-import-path harbor.kimetsu_agent:KimetsuAgent \
    -n 4
```

On Windows / PowerShell:

```powershell
$env:PYTHONPATH = (Get-Location).Path
$env:KIMETSU_BIN = "$pwd\target\release\kimetsu.exe"
harbor run `
  -d terminal-bench/terminal-bench-2 `
  --agent-import-path harbor.kimetsu_agent:KimetsuAgent `
  -n 4
```

## MP-7a is a stub agent; MP-7c will plumb the real pipeline

Today the Rust binary runs a one-shot echo through Harbor and emits
`agent.done`. That's deliberate — MP-7a exists to lock down the wire
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
  subprocess died. Inspect its stderr — the harbor-mode session never
  writes there for normal control flow, so any output is an error.
  Common cause: malformed JSON on the adapter's reply (check that
  PowerShell isn't inserting a UTF-16 BOM if you're scripting stdin).
- **`environment object has no exec`**: You're driving the adapter with
  something that doesn't quack like Harbor's environment. The smoke
  test demonstrates the minimum surface area.
