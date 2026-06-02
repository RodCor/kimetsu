# Plugin install: idempotent hook merge + workspace/global scope

**Date:** 2026-06-02
**Status:** Approved (pending spec review)
**Component:** `crates/kimetsu-chat/src/bridge.rs`, `crates/kimetsu-cli/src/main.rs`, `crates/kimetsu-chat/src/mcp_server.rs`

## Problem

`kimetsu plugin install` has two shortcomings:

1. **Hooks are replaced, not merged.** When a workspace (or user) already has
   hooks configured, the installer either errors (`already defines hooks; pass
   --force`) or, with `--force`, destroys the existing configuration:
   - `write_claude_hooks` rebuilds a fresh `hooks` object and does
     `root_obj.insert("hooks", …)`, replacing the **entire** hooks tree — every
     user hook on every event is lost.
   - `write_codex_hooks` overwrites the whole `UserPromptSubmit` array, dropping
     any non-Kimetsu entries on that event.

2. **Install is always workspace-relative.** Every surface is written under the
   workspace (`.claude/`, `.codex/`, `.mcp.json`). There is no way to install
   Kimetsu globally for all Claude Code / Codex projects.

## Goals

- Installing into a workspace/home that already has hooks **adds** Kimetsu's
  hooks alongside the existing ones; never removes or overwrites user hooks.
- Re-running install is **idempotent**: no duplicate Kimetsu hook groups, no
  `--force` required.
- A `--scope workspace|global` option (default `workspace`) lets the user
  install for the current workspace or globally for all Claude Code / Codex
  sessions. Applies to both harness targets.

## Non-goals

- No interactive TTY prompt; scope is a flag/argument (the npm launcher just
  forwards args to the native binary).
- No change to the hook command strings, brain logic, or proactive defaults.
- No new `user` vs `machine` scope distinction beyond workspace/global.

## Design

### 1. Idempotent, additive hook merge

A shared helper merges one Kimetsu hook group into one event array
(both Claude `settings.json` and Codex `hooks.json` use the same
`[{ matcher, hooks: [...] }]` shape):

```
merge_kimetsu_hook(event_array, kimetsu_group):
    # Kimetsu groups are identified by an inner command containing "kimetsu brain".
    if event_array contains a Kimetsu-owned group:
        replace that group in place   # picks up command / proactive changes
    else:
        append kimetsu_group           # preserve all existing entries
```

Applied to every event Kimetsu manages:
- Claude: `UserPromptSubmit`, `Stop`, and (when `proactive`) `PreToolUse`,
  `PostToolUse`.
- Codex: `UserPromptSubmit`, and (when `proactive`) `PreToolUse`, `PostToolUse`.

Consequences:
- Non-Kimetsu hooks on the same or other events are always preserved.
- Re-running install never duplicates Kimetsu groups (replace-in-place).
- The `already defines hooks; pass --force` errors are removed. Hook merge no
  longer consults `force`.

A Kimetsu-owned group is detected by scanning its `hooks[].command` strings for
the substring `kimetsu brain`.

### 2. MCP config writers become idempotent

`write_mcp_config` and `write_codex_config` currently error when a `kimetsu`
server is already present unless `--force`. Re-inserting the same `kimetsu`
server key is harmless, so these `--force` gates are removed; the writers always
upsert the `kimetsu` entry and preserve all other servers/keys.

### 3. Kimetsu-owned generated files auto-refresh

`bridge.md`, `delegate.md`, and the Codex `SKILL.md` are Kimetsu-authored, not
user content. Install always (re)writes them so the command is fully
re-runnable without `--force`. `CLAUDE.md` keeps its current behavior: written
only when missing, never clobbered.

`--force` is retained as an accepted flag (back-compat) but its only remaining
effect is overwriting an existing `CLAUDE.md`.

### 4. Scope resolution

New enum:

```rust
pub enum InstallScope { Workspace, Global }
// parse: "workspace" (default, also "ws"/"local"/""), "global" ("g"/"user")
```

`plugin_install` gains a `scope: InstallScope` parameter (inserted after
`target`). For `Global`, the home directory is resolved once via the existing
`USERPROFILE`/`HOME` helper and threaded into the path resolver, so tests can
inject a deterministic home (avoids the env-var race noted in the test-isolation
memory).

Path layout per (target, scope):

| Surface | Workspace | Global |
|---|---|---|
| Claude MCP | `<ws>/.mcp.json` (`servers` + `mcpServers`) | `~/.claude.json` (`mcpServers` only) |
| Claude commands | `<ws>/.claude/commands/kimetsu/` | `~/.claude/commands/kimetsu/` |
| Claude CLAUDE.md | `<ws>/.claude/CLAUDE.md` | `~/.claude/CLAUDE.md` |
| Claude hooks | `<ws>/.claude/settings.json` | `~/.claude/settings.json` |
| Codex config | `<ws>/.codex/config.toml` | `~/.codex/config.toml` |
| Codex skill | `<ws>/.codex/skills/kimetsu-bridge/` | `~/.codex/skills/kimetsu-bridge/` |
| Codex hooks | `<ws>/.codex/hooks.json` | `~/.codex/hooks.json` |

Notes:
- Hook command strings are unchanged. `--workspace .` / cwd-relative resolution
  is correct for a global server too, since `.` resolves to the project the
  harness launches in.
- Global Claude MCP merges into `~/.claude.json` `mcpServers` non-destructively
  (serde round-trip preserves all keys). The file is pretty-printed on write, so
  it is reformatted — accepted trade-off; it is the canonical user-scope MCP
  location.
- `BridgeTarget::Kimetsu` is unaffected by scope (no global concept).

### 5. Surface wiring

- **CLI** (`PluginInstallArgs`): add `#[arg(long, default_value = "workspace")]
  scope: String`; parse to `InstallScope`; pass to `plugin_install`. The install
  summary prints the scope alongside target and mode.
- **MCP** (`mcp_server.rs`): add a `scope` property
  (`enum ["workspace","global"]`, optional, default `workspace`) to the
  `kimetsu_plugin_install` input schema; parse in the handler; pass through.
  Update `PLUGIN_INSTALL_DESCRIPTION` to mention scope.
- Update the existing `plugin_install` call sites and unit tests for the new
  `scope` parameter.

## Testing

Unit tests in `bridge.rs`:

1. **Preserve user hooks (Claude):** seed `settings.json` with a non-Kimetsu
   `UserPromptSubmit` hook and a non-Kimetsu `PreToolUse` hook; after install
   both survive and Kimetsu's groups are present alongside.
2. **Preserve user hooks (Codex):** same for `hooks.json`.
3. **Idempotent re-run:** run install twice; assert exactly one Kimetsu group per
   event (no duplicates) for both targets.
4. **No --force needed:** install succeeds on a workspace that already has hooks,
   without `--force`.
5. **Global scope:** with an injected temp home, install writes under
   `home/.claude` + `home/.claude.json` (Claude) and `home/.codex` (Codex), and
   does **not** write under the workspace.

## Risks / trade-offs

- Reformatting `~/.claude.json` on global Claude MCP install (cosmetic).
- Kimetsu-group detection relies on the `kimetsu brain` command substring; this
  is stable across the codebase and is the same marker used elsewhere.
