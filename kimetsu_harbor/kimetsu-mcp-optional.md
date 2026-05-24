# Kimetsu MCP Optional Mode

Kimetsu MCP is available as an optional memory and brain sidecar.

For Terminal-Bench tasks, prefer `kimetsu_benchmark_context` first. Pass the task text, dataset, and `warm_policy="reactive_warm"` unless the run explicitly asks for a cold or full-warm condition. Kimetsu can detect the task slug, retrieve relevant memory when requested, and return a compact `playbook_markdown`.

Use `kimetsu_brain_context` for non-benchmark work when durable memory, prior outcomes, repo capsules, or workflow guidance would help the task. A good query is the current task goal plus key technologies.

Use `kimetsu_brain_status` when you need to inspect whether the brain is initialized or has useful memories. Use `kimetsu_skills_search` only when a portable skill may already exist.

If the host harness does not expose native MCP tools, you may use the mounted MCP stdio helper instead:

```bash
KIMETSU_TOOL_NAME=kimetsu_benchmark_context KIMETSU_BRAIN_WARM_POLICY=reactive_warm /mnt/e/Kimetsu/kimetsu_harbor/kimetsu-brain-context.sh "terminal-bench task goal and key technologies"
```

After a benchmark attempt, call `kimetsu_benchmark_record_outcome` with the pass/fail/error status, useful commands, pitfalls, and verification steps when available. If there is a reusable tactic or warning, include `generalized_memory` with `memory_role=semantic_operator` or `anti_pattern`; exact task details belong in the normal outcome fields. That is the memory loop Kimetsu contributes beyond plain MCP access.

If Kimetsu does not add value for this task, continue with the host harness's normal shell, file, edit, and verification tools.
