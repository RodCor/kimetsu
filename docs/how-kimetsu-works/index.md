
Kimetsu is a sidecar brain for coding agents. It runs alongside supported
host agents through MCP (including Claude Code and Codex), or as a standalone
chat REPL. It watches what the model does, learns which memories actually
help, and feeds higher-signal context into future runs. This document explains
the moving parts, in the order you'll encounter them.


## Ways to use it

**As a sidecar via MCP.** Run `kimetsu mcp serve` directly, or let
`kimetsu plugin install <target>` write the host config for you. The host
agent gets `kimetsu_*` tools (brain context + record, citations, memory
add/list/blame/conflicts, repo ingest, the bridge to other supported hosts).
Memories carry across sessions; learning compounds.

The intended loop is two calls: **`kimetsu_brain_context`** early on a
non-trivial task (zero overhead when the brain has nothing), then
**`kimetsu_brain_record`** after solving a problem worth remembering.
`kimetsu plugin install <host>` wires both automatically for **Claude Code**,
**Codex**, **Pi**, and **OpenClaw**, writing each host's native config (hooks
plus MCP; a TypeScript extension for Pi, which has no MCP). Hosts with a stop
event also get a session summary hook.

**As a standalone REPL.** Run `kimetsu chat`. Same brain, same
tools, only without a host harness. Useful for debugging a brain or
running short tasks where you don't want a second agent in the loop.

**As a shared server (Kimetsu Remote, beta).** Run the brain on a server and
connect over HTTP MCP: one brain per *repository*, shared across machines or a
team, with no local checkout. See §7a.

The CLI also has admin commands (`kimetsu brain ...`,
`kimetsu doctor`, `kimetsu bridge ...`) that you'll use for
maintenance, described below.

---
