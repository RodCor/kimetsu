"""Harbor Codex wrapper that enforces Kimetsu brain prefetch.

The built-in Harbor Codex agent can expose Kimetsu as MCP, but prompt-only
"required" mode still depends on the model deciding to call the tool. This
adapter moves the requirement outside the model loop: before Codex sees the
task, Harbor calls the Kimetsu MCP stdio helper, writes the raw response to the
trial logs, and prepends a compact brain-context block to the task prompt.
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any

try:  # pragma: no cover - only available in the WSL Harbor runtime
    from harbor.agents.installed.codex import Codex
    from harbor.environments.base import BaseEnvironment
    from harbor.models.agent.context import AgentContext
except Exception:  # noqa: BLE001
    Codex = object  # type: ignore[assignment,misc]
    BaseEnvironment = Any  # type: ignore[misc,assignment]
    AgentContext = Any  # type: ignore[misc,assignment]


DEFAULT_BUDGET_TOKENS = 2500


class KimetsuBrainPrefetchError(RuntimeError):
    """Raised when required brain context cannot be fetched."""


class CodexKimetsuRequired(Codex):  # type: ignore[misc,valid-type]
    """Codex agent with mandatory Kimetsu brain context.

    Environment knobs:
    - `KIMETSU_BRAIN_CONTEXT_HELPER`: path to `kimetsu-brain-context.sh`.
    - `KIMETSU_BRAIN_BUDGET_TOKENS`: helper budget, default 2500.
    - `KIMETSU_BENCHMARK_DATASET`: dataset name, default terminal-bench/terminal-bench-2.
    - `KIMETSU_BENCHMARK_TASK_SLUG`: optional task slug override.
    - `KIMETSU_BRAIN_WARM_POLICY`: `full_warm` (default), `reactive_warm`,
      or `cold_brain`.
    - `KIMETSU_REQUIRE_NONEMPTY_BRAIN`: set to `1` to fail on zero memory capsules.
    - `KIMETSU_REQUIRE_BENCHMARK_MEMORY`: set to `1` to fail unless a task-specific
      benchmark memory is retrieved.
    """

    @staticmethod
    def name() -> str:
        return "codex-kimetsu-required"

    def _env(self, key: str) -> str | None:
        getter = getattr(self, "_get_env", None)
        if callable(getter):
            return getter(key)
        return os.environ.get(key)

    def _helper_path(self) -> Path:
        explicit = self._env("KIMETSU_BRAIN_CONTEXT_HELPER")
        if explicit:
            return Path(explicit)
        return Path(__file__).resolve().parent / "kimetsu-brain-context.sh"

    def _budget_tokens(self) -> int:
        raw = self._env("KIMETSU_BRAIN_BUDGET_TOKENS") or str(DEFAULT_BUDGET_TOKENS)
        try:
            value = int(raw)
        except ValueError:
            value = DEFAULT_BUDGET_TOKENS
        return max(250, value)

    def _benchmark_dataset(self) -> str:
        return (
            self._env("KIMETSU_BENCHMARK_DATASET")
            or self._env("HARBOR_DATASET")
            or "terminal-bench/terminal-bench-2"
        )

    def _benchmark_task_slug(self) -> str | None:
        for key in (
            "KIMETSU_BENCHMARK_TASK_SLUG",
            "HARBOR_TASK_NAME",
            "HARBOR_TASK_ID",
            "TERMINAL_BENCH_TASK",
            "TB_TASK_ID",
            "TASK_NAME",
        ):
            value = self._env(key)
            if value:
                return value
        return self._task_slug_from_logs_dir()

    def _task_slug_from_logs_dir(self) -> str | None:
        try:
            logs_dir = Path(self.logs_dir)
        except Exception:  # noqa: BLE001
            return None

        for candidate in (logs_dir.name, logs_dir.parent.name):
            if "__" not in candidate:
                continue
            slug = candidate.split("__", 1)[0].strip()
            if slug:
                return slug
        return None

    def _warm_policy(self) -> str:
        value = (
            self._env("KIMETSU_BRAIN_WARM_POLICY")
            or self._env("KIMETSU_BENCHMARK_WARM_POLICY")
            or "full_warm"
        )
        normalized = value.strip().lower().replace("-", "_")
        if normalized in {"cold", "cold_brain", "brain_on_cold"}:
            return "cold_brain"
        if normalized in {"reactive", "reactive_warm", "warm_reactive", "optional_warm"}:
            return "reactive_warm"
        return "full_warm"

    @staticmethod
    def _build_query(instruction: str) -> str:
        compact = " ".join(instruction.split())
        if len(compact) > 1400:
            compact = compact[:1400].rstrip() + "..."
        return f"terminal-bench task: {compact}"

    async def _fetch_brain_context(self, instruction: str) -> dict[str, Any]:
        helper = self._helper_path()
        if not helper.is_file():
            raise KimetsuBrainPrefetchError(
                f"Kimetsu brain helper not found: {helper}"
            )

        env = os.environ.copy()
        env["KIMETSU_TOOL_NAME"] = "kimetsu_benchmark_context"
        env["KIMETSU_BUDGET_TOKENS"] = str(self._budget_tokens())
        env["KIMETSU_STAGE"] = "localization"
        env["KIMETSU_BENCHMARK_DATASET"] = self._benchmark_dataset()
        env["KIMETSU_BRAIN_WARM_POLICY"] = self._warm_policy()
        env["KIMETSU_MAX_CAPSULES"] = self._env("KIMETSU_MAX_CAPSULES") or "8"
        env["KIMETSU_REQUIRE_BENCHMARK_MEMORY"] = (
            self._env("KIMETSU_REQUIRE_BENCHMARK_MEMORY") or "0"
        )
        task_slug = self._benchmark_task_slug()
        if task_slug:
            env["KIMETSU_BENCHMARK_TASK_SLUG"] = task_slug
        query = self._build_query(instruction)

        proc = await asyncio.create_subprocess_exec(
            str(helper),
            query,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
        )
        stdout, stderr = await proc.communicate()
        stdout_text = stdout.decode("utf-8", errors="replace")
        stderr_text = stderr.decode("utf-8", errors="replace")

        self.logs_dir.mkdir(parents=True, exist_ok=True)
        (self.logs_dir / "kimetsu-brain-query.txt").write_text(
            query + "\n", encoding="utf-8"
        )
        (self.logs_dir / "kimetsu-brain-raw.json").write_text(
            stdout_text, encoding="utf-8"
        )
        if stderr_text:
            (self.logs_dir / "kimetsu-brain-stderr.txt").write_text(
                stderr_text, encoding="utf-8"
            )

        if proc.returncode != 0:
            raise KimetsuBrainPrefetchError(
                "Kimetsu brain helper failed "
                f"(exit {proc.returncode}): {stderr_text[:2000]}"
            )

        try:
            rpc_response = json.loads(stdout_text)
        except json.JSONDecodeError as exc:
            raise KimetsuBrainPrefetchError(
                f"Kimetsu brain helper returned invalid JSON: {exc}"
            ) from exc

        if "error" in rpc_response:
            raise KimetsuBrainPrefetchError(
                f"Kimetsu brain MCP error: {rpc_response['error']!r}"
            )

        text_payload = (
            rpc_response.get("result", {})
            .get("content", [{}])[0]
            .get("text", "{}")
        )
        try:
            brain_payload = json.loads(text_payload)
        except json.JSONDecodeError:
            brain_payload = {"ok": False, "raw_text": text_payload}

        (self.logs_dir / "kimetsu-brain-context.json").write_text(
            json.dumps(brain_payload, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )

        if not brain_payload.get("ok", False):
            raise KimetsuBrainPrefetchError(
                "Kimetsu benchmark context did not report ok=true"
            )

        require_nonempty = (self._env("KIMETSU_REQUIRE_NONEMPTY_BRAIN") or "").lower() in {
            "1",
            "true",
            "yes",
        }
        if require_nonempty and int(brain_payload.get("memory_capsule_count") or 0) == 0:
            raise KimetsuBrainPrefetchError(
                "Kimetsu benchmark context returned zero memory capsules"
            )

        require_benchmark = (
            self._env("KIMETSU_REQUIRE_BENCHMARK_MEMORY") or ""
        ).lower() in {
            "1",
            "true",
            "yes",
        }
        if require_benchmark and int(brain_payload.get("benchmark_memory_count") or 0) == 0:
            raise KimetsuBrainPrefetchError(
                "Kimetsu benchmark context returned zero task-specific benchmark memories"
            )

        return brain_payload

    @staticmethod
    def _format_brain_block(payload: dict[str, Any]) -> str:
        playbook = payload.get("playbook_markdown")
        if isinstance(playbook, str) and playbook.strip():
            return playbook.rstrip() + "\n\n"

        capsules = payload.get("capsules") or []
        lines = [
            "# Kimetsu Benchmark Context",
            "",
            "This context was fetched by Harbor before Codex started. Treat it as",
            "working memory and repo guidance for this task.",
            "",
            f"dataset: {payload.get('dataset', '')}",
            f"task_slug: {payload.get('task_slug', '')}",
            f"warm_policy: {payload.get('warm_policy', '')}",
            f"query: {payload.get('query', '')}",
            f"capsule_count: {payload.get('capsule_count', len(capsules))}",
            f"memory_capsule_count: {payload.get('memory_capsule_count', 0)}",
            f"benchmark_memory_count: {payload.get('benchmark_memory_count', 0)}",
            "",
        ]
        for idx, capsule in enumerate(capsules[:12], start=1):
            if not isinstance(capsule, dict):
                continue
            kind = capsule.get("kind", "capsule")
            summary = capsule.get("summary", "")
            handle = capsule.get("expansion_handle", "")
            score = capsule.get("score")
            score_text = f" score={score:.3f}" if isinstance(score, (int, float)) else ""
            lines.append(f"{idx}. [{kind}{score_text}] {summary}")
            if handle:
                lines.append(f"   source: {handle}")
        if len(capsules) > 12:
            lines.append(f"... {len(capsules) - 12} more capsules omitted.")
        lines.extend(["", "# Original Task", ""])
        return "\n".join(lines)

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        brain_payload = await self._fetch_brain_context(instruction)
        brain_block = self._format_brain_block(brain_payload)
        (self.logs_dir / "kimetsu-brain-context.md").write_text(
            brain_block + "\n", encoding="utf-8"
        )

        enriched_instruction = f"{brain_block}{instruction}"
        await super().run(enriched_instruction, environment, context)

        metadata = getattr(context, "metadata", None) or {}
        if not isinstance(metadata, dict):
            metadata = {"previous_metadata": metadata}
        metadata["kimetsu_brain_prefetch"] = {
            "ok": True,
            "tool": "kimetsu_benchmark_context",
            "query": brain_payload.get("query"),
            "task_slug": brain_payload.get("task_slug"),
            "warm_policy": brain_payload.get("warm_policy"),
            "capsule_count": brain_payload.get("capsule_count"),
            "memory_capsule_count": brain_payload.get("memory_capsule_count"),
            "benchmark_memory_count": brain_payload.get("benchmark_memory_count"),
            "log_files": [
                "kimetsu-brain-query.txt",
                "kimetsu-brain-raw.json",
                "kimetsu-brain-context.json",
                "kimetsu-brain-context.md",
            ],
        }
        try:
            context.metadata = metadata
        except Exception:  # noqa: BLE001
            pass
