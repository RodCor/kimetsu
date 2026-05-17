# MP-8 Verdict — Claude Code CLI is the wrong tool surface for v0.2

This is the honest write-up of what we discovered driving the v0.2
Terminal-Bench gauntlet end-to-end. Infrastructure works; the model
provider doesn't.

## What we built (and proved works)

Complete tooling chain from a Windows host:

| layer | result |
|-------|--------|
| Ubuntu 24.04 WSL2 distro (systemd PID 1) | clean install, default user `kimetsu` |
| Docker Engine 29.1.3 + Compose v2.40.3 | `systemctl enable --now docker`, daemon stable |
| Harbor 0.6.6 (pip) | runs Terminal-Bench 2 trials end-to-end |
| Linux kimetsu release binary | built from `/mnt/e/Kimetsu`, 13 MB ELF |
| Claude Code CLI 2.1.140 (npm `@anthropic-ai/claude-code`) | installed, authenticated via `CLAUDE_CODE_OAUTH_TOKEN` |
| `kimetsu_harbor` Python package | importable via `/usr/local/lib/python3.12/dist-packages/kimetsu.pth` |
| Python adapter ↔ Linux kimetsu smoke test | clean — 2 routed tool.exec frames + agent.done |
| `harbor run --agent-import-path kimetsu_harbor.kimetsu_agent:KimetsuAgent` | runs trials, calls model, scores via verifier |

Setup runbook: `kimetsu_harbor/SETUP-WSL.md` (committed earlier).

## The first real MP-8 data point

4-task slice on terminal-bench/terminal-bench-2 (`make-mips-interpreter`,
`circuit-fibsqrt`, `build-pov-ray`, `overfull-hbox`), 16m 44s, $1.37 cost:

| metric | value |
|--------|------:|
| trials completed end-to-end | 3 / 4 |
| trials timing out at 600s/call | 1 / 4 (build-pov-ray) |
| **mean reward** | **0.000** |
| n_input_tokens | 3,945 |
| n_output_tokens | 1,968 |
| cost_usd | 1.37 |

3 trials cleared the agent→verifier loop. Zero rewards is **not** a
hard-task problem — it's a protocol problem.

## Root cause (Claude Code CLI overrides our system prompt)

The kimetsu↔Claude-Code provider (from v0.1) speaks JSON envelopes:
the model is supposed to emit
`{"thought":"…","tool_call":{"name":"shell_command","input":{…}}}` and
`{"thought":"…","finish":{"summary":"…"}}`. v0.1 documented that
`claude_code::render_tool_protocol` appends an explicit grammar to the
system prompt; `apply_tool_envelope` parses the response back out.

In harbor mode the model **never emitted an envelope**. We sampled a
completed trial and got plain prose:

```
"I notice there's a discrepancy: the task description indicates I
 should use `shell_command` to interact with the workspace, but this
 tool is not available in my current environment. The available
 tools I have are:
 - Monitor (for watching long-running processes)
 - PushNotification (for sending notifications)
 - RemoteTrigger (for calling remote API triggers)
 ..."
```

Direct probe with the same `--system-prompt` flag through the raw
Claude Code CLI:

- `input_tokens: 7473` (our system prompt is ~2K — Claude Code added
  ~5K of its own)
- response: the model still claims `Monitor / PushNotification /
  RemoteTrigger` are its only tools
- `stop_reason: end_turn`, no `tool_use` block, no envelope

**Claude Code 2.x in `-p` print mode injects its own agentic harness
system prompt over whatever we pass via `--system-prompt`.** That
harness mentions its internal tools by name and effectively turns our
envelope grammar into a footnote the model ignores.

Confirmed via the CLI help:

> `--bare` — Minimal mode: skip hooks, LSP, plugin sync, attribution,
> auto-memory, background prefetches, keychain reads, and CLAUDE.md
> auto-discovery. **Anthropic auth is strictly `ANTHROPIC_API_KEY` or
> apiKeyHelper via --settings (OAuth and keychain are never read).**

`--bare` suppresses the harness — and explicitly disables OAuth. We
have `CLAUDE_CODE_OAUTH_TOKEN` (subscription path), not
`ANTHROPIC_API_KEY` (the user has stated they cannot use the API path),
so `--bare` is out.

## Fixes shipped this session (still worth keeping)

Committed to `main`:

- WSL setup runbook (`kimetsu_harbor/SETUP-WSL.md`)
- Adapter package rename `harbor/` → `kimetsu_harbor/` to avoid
  Python module collision with the Harbor framework
- Adapter contract fixes: `__init__` accepts Harbor's `logs_dir` +
  kwargs, populates `context.metadata` (pydantic), uses
  `environment.exec(timeout_sec=…)` shape
- `kimetsu.pth` install so Harbor's importer finds the adapter without
  PYTHONPATH

Local-only (not yet committed) tactical fixes that didn't unlock the
underlying problem but were worth doing:

- `claude_code.rs`: `--max-turns 1` → `8` so Claude Code's inner loop
  doesn't hit `error_max_turns` on a single user turn
- `kimetsu-cli/src/main.rs`: harbor-mode model timeout 180s → 600s so
  complex tasks aren't killed mid-thinking
- `kimetsu_harbor/kimetsu_agent.py`: log kimetsu's stderr when it
  exits before `agent.done` so we can see the real failure instead of
  `"exited before emitting agent.done"`
- `harbor.rs`: rewrite harbor-mode system prompt so it doesn't
  contradict `render_tool_protocol` ("respond with plain text" was
  fighting "output exactly one JSON object")

None of those fix the core issue — Claude Code's harness injection
swallows our tool-call protocol regardless.

## Three paths forward (pick one for MP-9)

| path | description | cost | unlocks |
|------|-------------|-----:|---------|
| **A. MCP server proxy** | Write a tiny Rust MCP server child-process that exposes `shell_command` to Claude Code via `--mcp-config`. The MCP server proxies calls back to kimetsu's `HarborSession`. | 1-2 days | Native Claude-Code tool calling; survives the harness injection because MCP tools ARE part of the harness. |
| **B. v0.1 pipeline through HarborShellExecutor** | Drop the `run_model_agent` envelope approach. Wire the existing `run_coding` (or a slim version) so all tool execution flows through `HarborShellExecutor` — the kimetsu v0.1 envelope contract still works because v0.1 doesn't use Claude Code as an "agentic harness", just as a chat completion endpoint that returns text. We then have to verify that's actually still how the v0.1 bench works in 2.x. | 1-3 days | The whole v0.1 brain (broker, prior_run capsules, curated memory) flows into Terminal-Bench, which is the actual v0.2 falsifiable claim. |
| **C. Accept Claude Code's native tools** | Configure `--allowedTools "Bash"` so the model uses Claude Code's `Bash` tool natively. Intercept that via a fake `/bin/bash` wrapper that routes through HarborSession. | 0.5 day | Quick path to non-zero rewards, but architecturally fragile and tightly coupled to Claude Code's internals. |

**Recommendation: Path B.** It re-uses the v0.1 envelope contract that
already produced a 56% MVP claim pass, and it routes the entire brain
through HarborShellExecutor — which is exactly the v0.2 falsifiable
test we want. Path A is the most architecturally pure but has a real
learning-curve / debug cost on the MCP protocol. Path C is fastest but
buys us the wrong thing.

## What v0.2 ship-gate becomes after MP-8

Before MP-8 we wanted "3 runs over 1 week within ±5pp on the three-mode
gauntlet". Now we know Terminal-Bench will give us numbers as soon as
**the model actually invokes shell_command**. So the v0.2 ship gate
shifts to:

1. Pick path A, B, or C and land it as MP-9.
2. Re-run the 4-task gauntlet → non-zero reward proves the agent loop
   is doing real work.
3. Scale to a larger slice (16-32 tasks) for a real baseline.
4. Add the kimetsu-brain mode (broker + curated memories) and verify
   `kimetsu-brain ≥ kimetsu-no-brain` against that baseline.

Until MP-9 lands the gauntlet remains uninformative — we already know
the answer is 0%.

## Artifacts

- Job dir: `/home/kimetsu/harbor-jobs/jobs/kimetsu-mp8/` (Ubuntu 24.04
  WSL), 4 trials with full `result.json` + `exception_info.exception_message`
- Setup runbook: `kimetsu_harbor/SETUP-WSL.md`
- Adapter package: `kimetsu_harbor/`
- Rust harbor module: `crates/kimetsu-agent/src/harbor.rs`
