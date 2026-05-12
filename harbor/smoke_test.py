"""Smoke test for harbor/kimetsu_agent.py — exercises the adapter ↔
kimetsu binary handshake without requiring Harbor or Docker installed.

Run:
    python harbor/smoke_test.py            # uses target/debug/kimetsu
    KIMETSU_BIN=target/release/kimetsu.exe python harbor/smoke_test.py

Exit 0 on success, non-zero with a stack trace on any mismatch.

This is a manual test (not unittest / pytest) so it can be invoked on
any machine with a Python interpreter and a built kimetsu binary, with
no external Python deps. Once Harbor itself is installed, the same flow
runs under `harbor run --agent-import-path harbor.kimetsu_agent:KimetsuAgent`.
"""

from __future__ import annotations

import asyncio
import os
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

# Make sure we can import the adapter when running from the repo root.
THIS_DIR = Path(__file__).resolve().parent
REPO_ROOT = THIS_DIR.parent
sys.path.insert(0, str(REPO_ROOT))

from harbor.kimetsu_agent import KimetsuAgent, resolve_kimetsu_binary  # noqa: E402


@dataclass
class FakeEnvironment:
    """Mock of Harbor's environment.exec — runs commands locally via
    subprocess.run so we can verify the adapter forwards the kimetsu
    JSON-RPC tool.exec call into the host shell unchanged."""
    calls: list[str] = field(default_factory=list)

    def exec(self, cmd: str, cwd: str | None = None) -> dict[str, object]:
        self.calls.append(cmd)
        # We don't actually need to run the command for the stub agent
        # (it only sends one echo), but doing so proves the round-trip.
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
    binary = resolve_kimetsu_binary()
    print(f"[smoke] using kimetsu binary: {binary}")

    env = FakeEnvironment()
    context: dict[str, object] = {}
    agent = KimetsuAgent()

    await agent.run(
        instruction="harbor adapter smoke test",
        environment=env,
        context=context,
    )

    # Adapter populates context with summary, tool_calls, protocol_version.
    print(f"[smoke] context keys: {list(context.keys())}")
    print(f"[smoke] summary: {context.get('summary')!r}")
    print(f"[smoke] tool_calls: {context.get('tool_calls')}")
    print(f"[smoke] environment received {len(env.calls)} command(s)")

    # Validate: exactly one tool.exec for the stub agent's echo, agent.done
    # populated summary that mentions our task and the protocol version.
    summary = str(context.get("summary") or "")
    assert "harbor adapter smoke test" in summary, summary
    assert "protocol=0.1" in summary, summary
    assert len(env.calls) == 1, env.calls
    assert env.calls[0].startswith("echo"), env.calls
    tool_calls = context.get("tool_calls") or []
    assert isinstance(tool_calls, list) and len(tool_calls) == 1
    assert tool_calls[0]["program"] == "echo"
    assert tool_calls[0]["exit_code"] == 0
    assert context.get("protocol_version") == "0.1"

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
