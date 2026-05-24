"""Smoke test for kimetsu_harbor/kimetsu_agent.py â€” exercises the adapter â†”
kimetsu-harbor-agent handshake without requiring Harbor or Docker installed.

Run:
    python kimetsu_harbor/smoke_test.py
    KIMETSU_HARBOR_BIN=target/release/kimetsu-harbor-agent.exe python kimetsu_harbor/smoke_test.py

Exit 0 on success, non-zero with a stack trace on any mismatch.

This is a manual test (not unittest / pytest) so it can be invoked on
any machine with a Python interpreter and a built kimetsu-harbor-agent binary, with
no external Python deps. Once Harbor itself is installed, the same flow
runs under `harbor run --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent`.
"""

from __future__ import annotations

import asyncio
import os
import platform
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

# Make sure we can import the adapter when running from the repo root.
THIS_DIR = Path(__file__).resolve().parent
REPO_ROOT = THIS_DIR.parent
sys.path.insert(0, str(REPO_ROOT))

from kimetsu_harbor.kimetsu_agent import KimetsuAgent, resolve_harbor_agent_binary  # noqa: E402


@dataclass
class FakeEnvironment:
    """Mock of Harbor's environment.exec â€” runs commands locally via
    subprocess.run so we can verify the adapter forwards the kimetsu
    JSON-RPC tool.exec call into the host shell unchanged."""
    calls: list[str] = field(default_factory=list)

    def exec(self, cmd: str, cwd: str | None = None) -> dict[str, object]:
        self.calls.append(cmd)
        # We don't actually need to run the command for the stub agent
        # (it only sends one echo), but doing so proves the round-trip.
        if platform.system() == "Windows":
            completed = subprocess.run(
                ["powershell", "-NoProfile", "-Command", cmd],
                capture_output=True,
                text=True,
                cwd=cwd,
            )
        else:
            completed = subprocess.run(
                cmd,
                shell=True,
                capture_output=True,
                text=True,
                cwd=cwd,
            )
        return {
            "exit_code": completed.returncode,
            "stdout": completed.stdout,
            "stderr": completed.stderr,
        }


async def main() -> int:
    binary = resolve_harbor_agent_binary()
    print(f"[smoke] using harbor agent binary: {binary}")

    # MP-7d: force the stub mode so the smoke test doesn't require API
    # credentials. The real model agent runs without this env var when
    # Harbor invokes us (CLAUDE_CODE_OAUTH_TOKEN must be set then).
    os.environ["KIMETSU_HARBOR_STUB"] = "1"

    env = FakeEnvironment()
    # When Harbor is installed, AgentContext is a pydantic model — use
    # it so the adapter exercises the real attribute path. Fall back to
    # a plain dict when Harbor isn't available (e.g. CI without
    # `pip install harbor`).
    try:
        from harbor.models.agent.context import AgentContext  # type: ignore
        context: Any = AgentContext()
    except Exception:  # noqa: BLE001
        context = {}

    # Harbor's BaseAgent requires logs_dir; satisfy it with a scratch
    # path so KimetsuAgent() works whether or not Harbor is installed.
    logs_dir = Path(os.environ.get("TMPDIR", "/tmp")) / "kimetsu-harbor-smoke"
    logs_dir.mkdir(parents=True, exist_ok=True)
    try:
        agent = KimetsuAgent(logs_dir=logs_dir)
    except TypeError:
        # Local stub BaseAgent doesn't accept kwargs.
        agent = KimetsuAgent()

    await agent.run(
        instruction="harbor adapter smoke test",
        environment=env,
        context=context,
    )

    # Adapter populates either context.metadata (Harbor's AgentContext)
    # or the fallback dict shape.
    if hasattr(context, "metadata"):
        meta = context.metadata or {}
    elif hasattr(context, "__contains__"):
        meta = context.get("metadata") or context  # legacy / dict fallback
    else:
        meta = {}
    print(f"[smoke] metadata keys: {list(meta.keys())}")
    print(f"[smoke] summary: {meta.get('summary')!r}")
    print(f"[smoke] tool_calls: {meta.get('tool_calls')}")
    print(f"[smoke] environment received {len(env.calls)} command(s)")

    summary = str(meta.get("summary") or "")
    assert "harbor adapter smoke test" in summary, summary
    assert "protocol=0.1" in summary, summary
    assert len(env.calls) == 2, env.calls
    assert env.calls[0].startswith("pwd"), env.calls
    assert env.calls[1].startswith("echo"), env.calls
    tool_calls = meta.get("tool_calls") or []
    assert isinstance(tool_calls, list) and len(tool_calls) == 2
    assert [tc["program"] for tc in tool_calls] == ["pwd", "echo"]
    assert all(tc["exit_code"] == 0 for tc in tool_calls), tool_calls
    assert meta.get("protocol_version") == "0.1"

    kctx = meta.get("kimetsu_context") or {}
    assert kctx.get("stub") == "multi-step", kctx
    steps = kctx.get("steps") or []
    assert [s["program"] for s in steps] == ["pwd", "echo"], steps

    print("[smoke] OK")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except AssertionError as exc:
        print(f"[smoke] FAIL: assertion {exc!r}", file=sys.stderr)
        sys.exit(1)
    except Exception as exc:
        print(f"[smoke] FAIL: {type(exc).__name__}: {exc}", file=sys.stderr)
        raise
