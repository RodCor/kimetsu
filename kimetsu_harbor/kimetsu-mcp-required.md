# Kimetsu MCP Required Mode

This run is measuring Kimetsu brain usage. Before inspecting files, running shell commands, editing, or planning in detail, retrieve Kimetsu brain context.

When launched through `run-codex-kimetsu-bench.sh`, Harbor should already have
prefetched Kimetsu benchmark context and inserted a `# Kimetsu Benchmark
Playbook` block above the original task. Treat that block as required working
context. If the block is present, do not spend extra time proving the
requirement before starting the task.

If no `# Kimetsu Benchmark Playbook` block was injected by the harness, your
mandatory first action is:

1. Call `kimetsu_benchmark_context` with the full benchmark task text, the dataset `terminal-bench/terminal-bench-2`, and `warm_policy="full_warm"` unless the run explicitly asks for `cold_brain` or `reactive_warm`.
2. Read and incorporate the returned `playbook_markdown` before continuing.
3. If `kimetsu_benchmark_context` is unavailable, fall back to `kimetsu_brain_context` with a concise query containing the benchmark task goal and key technologies.

If the host harness does not expose native MCP tools, do not stop after saying the tool is unavailable. Immediately use the mounted MCP stdio helper instead:

```bash
KIMETSU_TOOL_NAME=kimetsu_benchmark_context KIMETSU_BRAIN_WARM_POLICY=full_warm /mnt/e/Kimetsu/kimetsu_harbor/kimetsu-brain-context.sh "terminal-bench <task name> <task goal> <key technologies>"
```

Replace the placeholder text with a concise query for the current task. Do not reuse an example query from another benchmark task.

That helper sends a JSON-RPC `tools/call` request to the Kimetsu MCP server over stdio and prints the `kimetsu_benchmark_context` result. Treat that output exactly like the native MCP tool result.

After the attempt, call `kimetsu_benchmark_record_outcome` when the tool is available and include the task slug, pass/fail/error status, key commands, pitfalls, and verification command. If the run reveals a reusable tactic or warning, include `generalized_memory` with `memory_role=semantic_operator` or `anti_pattern`; keep exact task details in the normal outcome fields. This is how Kimetsu turns benchmark runs into cheaper future playbooks without overfitting one task slug.

Use `kimetsu_brain_status` if you need to confirm initialization or memory availability. Use `kimetsu_skills_search` if the task may match a portable Kimetsu, Codex, or Claude Code skill.

Do not satisfy this requirement by running arbitrary `kimetsu` CLI commands. Use either the native MCP tool or the provided MCP stdio helper so the benchmark transcript shows that Kimetsu brain context was requested.
