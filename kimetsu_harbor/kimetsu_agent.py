"""Kimetsu â†” Harbor adapter (MP-7b).

Bridges the Rust `kimetsu agent --harbor-mode` binary into Harbor's
external-agent interface so Terminal-Bench can grade Kimetsu against
the same tasks it grades Claude Code / Cursor / etc. on.

The contract is:

  Harbor              â”€ instruction, environment â”€â”€> KimetsuAgent.run
  KimetsuAgent.run    â”€ spawns kimetsu agent --harbor-mode --task â”€> subprocess
  kimetsu subprocess  â”€ JSON-RPC tool.exec on stdout â”€â”€>            this adapter
  this adapter        â”€ environment.exec(...) â”€â”€>                   Harbor environment
  Harbor environment  â”€ {exit_code, stdout, stderr} â”€â”€>             this adapter
  this adapter        â”€ JSON-RPC reply on subprocess.stdin â”€â”€>      kimetsu subprocess
  kimetsu subprocess  â”€ JSON-RPC agent.done on stdout â”€â”€>           this adapter
  KimetsuAgent.run    â”€ populated context â”€â”€>                       Harbor

Protocol details live in `crates/kimetsu-agent/src/harbor.rs`. Only
`tool.exec` and `agent.done` exist in v0.2; read/write/diff are all
expressed as shell commands so the adapter only needs to translate one
method.

Compatibility notes:
- Harbor's `environment.exec` shape is not fully spec'd in the public
  docs. We probe for a few common return-value shapes (tuple of three,
  dict, dataclass with `.exit_code/.stdout/.stderr` attrs) and fall
  back to safe defaults. Update `_normalize_exec_result` below when the
  exact contract is pinned down.
- Harbor's `BaseAgent.run` is documented as `async`, so this adapter
  uses asyncio. The kimetsu binary itself is sync (line-by-line JSON
  on stdin/stdout), but asyncio.create_subprocess_exec wraps it so we
  can `await` reads without blocking Harbor's event loop.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
from dataclasses import dataclass
from typing import Any, Iterable, Mapping

logger = logging.getLogger(__name__)

# --- BaseAgent shim ---------------------------------------------------------
# Harbor's BaseAgent is part of the harness install. We import it lazily so
# `from kimetsu_harbor.kimetsu_agent import KimetsuAgent` still works in environments
# without Harbor installed (e.g. our smoke test, lint tooling). If Harbor
# isn't present we fall back to a no-op stub so type checking and smoke
# tests still pass.

try:  # pragma: no cover - exercised only when Harbor is installed
    from harbor.agents.base import BaseAgent  # type: ignore
except Exception:  # noqa: BLE001
    class BaseAgent:  # type: ignore[no-redef]
        """Local stub so the module imports without Harbor installed.

        Harbor replaces this with the real BaseAgent at runtime. Tests that
        drive `KimetsuAgent` directly (without going through Harbor's
        registry) use this stub and supply their own `environment`.
        """

        @staticmethod
        def name() -> str:  # pragma: no cover
            return "base-agent-stub"

        def version(self) -> str:  # pragma: no cover
            return "stub"


# --- Configuration ---------------------------------------------------------

DEFAULT_KIMETSU_BIN_CANDIDATES: tuple[str, ...] = (
    "kimetsu",
    "kimetsu.exe",
    "/usr/local/bin/kimetsu",
    "target/release/kimetsu",
    "target/release/kimetsu.exe",
    "target/debug/kimetsu",
    "target/debug/kimetsu.exe",
)

PROTOCOL_VERSION = "0.1"


def resolve_kimetsu_binary() -> str:
    """Find the kimetsu binary. KIMETSU_BIN env var wins; then we walk a
    fixed list of common locations relative to CWD; finally we fall back
    to whatever `shutil.which` finds on PATH. Raises if nothing matches
    so the bench fails loudly rather than hanging."""
    override = os.environ.get("KIMETSU_BIN")
    if override:
        if not os.path.isfile(override):
            raise FileNotFoundError(
                f"KIMETSU_BIN={override!r} does not exist or is not a file"
            )
        return override

    for candidate in DEFAULT_KIMETSU_BIN_CANDIDATES:
        path = shutil.which(candidate)
        if path:
            return path
        if os.path.isfile(candidate):
            return os.path.abspath(candidate)

    raise FileNotFoundError(
        "could not locate the kimetsu binary; set KIMETSU_BIN or build "
        "with `cargo build -p kimetsu-cli --release`"
    )


# --- Exec-result normalization --------------------------------------------

@dataclass(frozen=True)
class ExecResult:
    exit_code: int
    stdout: str
    stderr: str
    timed_out: bool = False


def _normalize_exec_result(raw: Any) -> ExecResult:
    """Coerce Harbor's environment.exec return value into the shape
    kimetsu's tool.exec result expects.

    Known shapes (updated as we learn more):
    - 3-tuple `(stdout, stderr, exit_code)`
    - 3-tuple `(exit_code, stdout, stderr)` (less likely but cheap to guard)
    - dict with `exit_code`/`stdout`/`stderr` keys
    - object with `.exit_code`, `.stdout`, `.stderr` attrs (Harbor's
      typed dataclass shape if it exists)
    """
    if isinstance(raw, ExecResult):
        return raw

    # Dict / mapping shape
    if isinstance(raw, Mapping):
        return ExecResult(
            exit_code=int(raw.get("exit_code", raw.get("returncode", 0)) or 0),
            stdout=str(raw.get("stdout", "") or ""),
            stderr=str(raw.get("stderr", "") or ""),
            timed_out=bool(raw.get("timed_out", False)),
        )

    # Tuple / list shape
    if isinstance(raw, (tuple, list)) and len(raw) >= 3:
        a, b, c = raw[0], raw[1], raw[2]
        # If the first element looks like an int, assume (code, stdout, stderr)
        if isinstance(a, int) and isinstance(b, str) and isinstance(c, str):
            return ExecResult(exit_code=int(a), stdout=b, stderr=c)
        # Otherwise assume (stdout, stderr, code)
        if isinstance(c, int):
            return ExecResult(
                exit_code=int(c),
                stdout=str(a) if a is not None else "",
                stderr=str(b) if b is not None else "",
            )

    # Object with attrs
    exit_code = getattr(raw, "exit_code", None)
    if exit_code is None:
        exit_code = getattr(raw, "returncode", None)
    if exit_code is not None:
        return ExecResult(
            exit_code=int(exit_code),
            stdout=str(getattr(raw, "stdout", "") or ""),
            stderr=str(getattr(raw, "stderr", "") or ""),
            timed_out=bool(getattr(raw, "timed_out", False)),
        )

    # Unknown shape: log and return a hard failure so the model sees the
    # tool call as broken instead of silently succeeding with empty output.
    logger.error("could not normalize environment.exec result: %r", raw)
    return ExecResult(
        exit_code=255,
        stdout="",
        stderr=f"harbor adapter: unrecognized environment.exec return shape: {raw!r}",
    )


def _format_shell_command(program: str, args: Iterable[str]) -> str:
    """Render `(program, args)` as a single shell command string. Harbor's
    environment.exec is widely documented as taking a single command
    string (it runs inside a bash shell). We use simple POSIX quoting via
    shlex.quote so paths and args with spaces survive."""
    import shlex

    parts = [shlex.quote(program)]
    parts.extend(shlex.quote(a) for a in args)
    return " ".join(parts)


# --- The adapter -----------------------------------------------------------

class KimetsuAgent(BaseAgent):
    """Harbor external-agent wrapper around `kimetsu agent --harbor-mode`.

    Mode selection (MP-7d):
    - default: spawn kimetsu with the real model loop (claude_code
      provider). `CLAUDE_CODE_OAUTH_TOKEN` must be set in the
      environment when the kimetsu binary starts.
    - `KIMETSU_HARBOR_STUB=1`: pass `--stub` so the binary runs the
      protocol-only multi-step stub (no model calls). Used by the smoke
      test on machines without API credentials.

    `KIMETSU_HARBOR_MODEL` (optional) overrides the model id; default is
    whatever the binary's --model flag falls back to (`claude-haiku-4-5`).
    """

    @staticmethod
    def name() -> str:
        return "kimetsu"

    def version(self) -> str:  # pragma: no cover - trivial
        return "0.2.0"

    async def setup(self, environment: Any) -> None:  # pragma: no cover
        """Harbor calls this before run(). Kimetsu's binary is expected to
        be present on the host (we don't install into the Harbor docker
        image in v0.2). Defer the resolve to run() so a bad config fails
        loudly per-task instead of at registry import."""
        return None

    async def run(
        self,
        instruction: str,
        environment: Any,
        context: Any,
    ) -> None:
        """Drive one Terminal-Bench task end-to-end.

        Spawns the kimetsu binary, pumps JSON-RPC frames until we receive
        `agent.done`, then returns. `context` is whatever Harbor expects
        us to populate; we treat it as a mapping if possible and fall back
        to setting attributes otherwise.
        """
        binary = resolve_kimetsu_binary()
        argv = [binary, "agent", "--harbor-mode", "--task", instruction]
        if os.environ.get("KIMETSU_HARBOR_STUB"):
            argv.append("--stub")
        model_override = os.environ.get("KIMETSU_HARBOR_MODEL")
        if model_override:
            argv.extend(["--model", model_override])

        logger.info("starting kimetsu agent: argv=%r", argv)

        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        assert proc.stdout is not None and proc.stdin is not None

        summary: str | None = None
        agent_context_payload: Any = None
        tool_calls: list[dict[str, Any]] = []

        try:
            while True:
                line = await proc.stdout.readline()
                if not line:
                    # Subprocess closed stdout without sending agent.done
                    raise RuntimeError(
                        "kimetsu agent exited before emitting agent.done"
                    )
                try:
                    msg = json.loads(line.decode("utf-8"))
                except json.JSONDecodeError:
                    logger.warning("non-JSON line on kimetsu stdout: %r", line[:200])
                    continue

                method = msg.get("method")
                if method == "tool.exec":
                    params = msg.get("params", {}) or {}
                    program = params.get("program", "")
                    args = params.get("args", []) or []
                    cwd = params.get("cwd")
                    cmd = _format_shell_command(program, args)
                    logger.debug("forwarding tool.exec to environment: %s (cwd=%s)", cmd, cwd)

                    exec_result = await self._invoke_environment_exec(
                        environment, cmd, cwd=cwd
                    )
                    tool_calls.append({
                        "program": program,
                        "args": list(args),
                        "exit_code": exec_result.exit_code,
                    })

                    reply = {
                        "jsonrpc": "2.0",
                        "id": msg.get("id"),
                        "result": {
                            "exit_code": exec_result.exit_code,
                            "stdout": exec_result.stdout,
                            "stderr": exec_result.stderr,
                            "timed_out": exec_result.timed_out,
                        },
                    }
                    proc.stdin.write((json.dumps(reply) + "\n").encode("utf-8"))
                    await proc.stdin.drain()
                elif method == "agent.done":
                    params = msg.get("params", {}) or {}
                    summary = params.get("summary")
                    agent_context_payload = params.get("context")
                    break
                else:
                    logger.warning("unhandled JSON-RPC method from kimetsu: %r", method)

        finally:
            # Close stdin so the subprocess exits cleanly even on error
            try:
                proc.stdin.close()
            except Exception:  # noqa: BLE001
                pass
            try:
                await asyncio.wait_for(proc.wait(), timeout=10.0)
            except asyncio.TimeoutError:  # pragma: no cover - defensive
                proc.kill()
                await proc.wait()

        # Populate Harbor's AgentContext (pydantic v2 BaseModel). Its
        # documented fields are n_input_tokens / n_cache_tokens /
        # n_output_tokens / cost_usd / rollout_details / metadata. We
        # store our payload under `metadata` (the catch-all dict), and
        # lift token / cost counters from the kimetsu-side context when
        # MP-7d's run_model_agent populates them. Pydantic v2 also
        # supports attribute assignment, so direct setattr works.
        metadata = {
            "summary": summary or "",
            "tool_calls": tool_calls,
            "kimetsu_context": agent_context_payload,
            "protocol_version": PROTOCOL_VERSION,
        }
        try:
            context.metadata = metadata
        except Exception:  # noqa: BLE001 - non-pydantic fallback
            try:
                context["metadata"] = metadata
            except Exception:  # noqa: BLE001
                setattr(context, "kimetsu_metadata", metadata)

        # Lift token / cost from the kimetsu-side agent.done context if
        # MP-7d populated them. Best-effort: missing keys leave the
        # AgentContext fields at their default None.
        if isinstance(agent_context_payload, dict):
            for src_key, dst_attr in (
                ("input_tokens", "n_input_tokens"),
                ("output_tokens", "n_output_tokens"),
                ("cost_usd", "cost_usd"),
            ):
                value = agent_context_payload.get(src_key)
                if value is not None:
                    try:
                        setattr(context, dst_attr, value)
                    except Exception:  # noqa: BLE001
                        pass

    async def _invoke_environment_exec(
        self,
        environment: Any,
        cmd: str,
        *,
        cwd: str | None,
    ) -> ExecResult:
        """Best-effort call into Harbor's environment.exec.

        Real Harbor environments expose `exec(cmd: str, **kwargs)` and the
        result may be sync or async. We probe both and normalize.
        """
        if not hasattr(environment, "exec"):
            return ExecResult(
                exit_code=255,
                stdout="",
                stderr=(
                    "harbor adapter: environment object has no `exec`; "
                    "got " + type(environment).__name__
                ),
            )

        # Harbor's BaseEnvironment.exec signature (v0.6.x):
        #     exec(command: str, cwd: str | None = None,
        #          env: dict[str, str] | None = None,
        #          timeout_sec: int | None = None,
        #          user: str | int | None = None) -> ExecResult
        # We probe for kwarg support so older / forked environments still
        # work with a positional-only call.
        kwargs: dict[str, Any] = {}
        if cwd:
            kwargs["cwd"] = cwd
        try:
            raw = environment.exec(cmd, **kwargs)
        except TypeError:
            raw = environment.exec(cmd)
        if asyncio.iscoroutine(raw):
            raw = await raw
        return _normalize_exec_result(raw)
