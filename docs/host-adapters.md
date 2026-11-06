# Host adapters (ADR 0001 F4)

MCP is the edge between a coding host and the Kimetsu brain. The installer writes a host file that starts `kimetsu mcp serve`. It does not open `brain.db`, and it does not add a second protocol. The in-process Rust SDK stays next to SQLite. `@kimetsu-ai/sdk` remains an HTTP client of Kimetsu Remote.

```bash
kimetsu plugin install claude-code
kimetsu plugin install codex
kimetsu plugin install cursor
kimetsu plugin install grok-bot
kimetsu plugin install grok-bot --remote https://kimetsu.example.com:8787
```

`scope` is `workspace` (default) or `global`. `mode` is `optional` or `required`.

## What each host gets

| Host | File | Shape checked |
| --- | --- | --- |
| Claude Code | `.mcp.json`, or `~/.claude.json` when global | `mcpServers.kimetsu` with `type` `stdio`, `command` `kimetsu`, `args` `["mcp", "serve", "--workspace", "."]`. Workspace files also keep the older `servers` key. |
| Codex | `.codex/config.toml` or `~/.codex/config.toml` | `[mcp_servers.kimetsu]` `command` and `args` for that same server. |
| Cursor | `.cursor/mcp.json` or `~/.cursor/mcp.json` | `mcpServers.kimetsu` `type` `stdio` with the same command and args. Workspace install also writes `.cursor/rules/kimetsu-brain/rule.md`. |
| Grok Bot | `.kimetsu/hosts/grok-bot.md` | Operator snippet. Not a Grok Bot settings file. |

Sources read 2026-11-06:

- Claude Code: <https://code.claude.com/docs/en/mcp>
- Codex: <https://developers.openai.com/codex/mcp>
- Cursor: <https://cursor.com/docs/mcp>
- Grok Bot: <https://docs.x.ai/grok-bot/computer-and-apps>

Grok Build's `~/.grok/config.toml` is a different product (<https://docs.x.ai/build/features/mcp-servers>). This installer does not write it.

## Operator workflow

Stdio grants are explicit:

- Read `repo` and read `user`.
- Write `repo` only, and only while write tools are enabled. Locally that defaults on. Remote write tools stay off unless `KIMETSU_MCP_ENABLE_WRITE_TOOLS=1`.

`kimetsu_brain_context` returns capsules from those read grants. The call uses stored evidence. It does not call a generative model. A warm-start block, when the server includes one, is that same digest.

`kimetsu_brain_record` stores the lesson on `repo`. A `level` or `scope` argument does not move it to `user`, `org`, or `skill`.

`kimetsu_brain_cite` needs `exposure_id` from the context response and `memory_id` from a delivered capsule. `outcome` is `helped`, `noop`, or `harmed`. Omitting it records `noop`. Only `helped` raises usefulness. `harmed` removes the memory from injection (`invalidated_reason` `cite_harmed`). A citation records reliance, not verification.

Successful record and cite responses include `grants.read` `["repo", "user"]` and `grants.write` `["repo"]`.

## Limits

These checks generate the files and call the MCP server in-process. They do not start Claude Code, Codex, Cursor, or Grok Bot. Claude Code still asks a person to approve a project `.mcp.json`. Grok Bot's chat confirmation was not run. Adding a server on a private network, or one that uses a private certificate authority, asks for approval on that host. Pi and OpenClaw installs are unchanged.
