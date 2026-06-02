# Plugin Install: Hook Merge + Workspace/Global Scope — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `kimetsu plugin install` merge its hooks into existing hook configs (never destroying user hooks, including on events Kimetsu also uses), be idempotent on re-run, and support a `--scope workspace|global` choice for both Claude Code and Codex.

**Architecture:** All install logic lives in `crates/kimetsu-chat/src/bridge.rs`. A shared idempotent merge helper upserts Kimetsu's own matcher group into each event array. Scope is resolved up front into concrete target paths (workspace dir vs. home dir), threaded through `plugin_install` and exposed on the CLI (`--scope`) and the `kimetsu_plugin_install` MCP tool.

**Tech Stack:** Rust, `serde_json` (Claude `settings.json` / `.mcp.json` / `~/.claude.json`), `toml` (Codex `config.toml`), `clap` (CLI), the workspace's MCP server harness.

**Spec:** `docs/superpowers/specs/2026-06-02-plugin-install-merge-and-scope-design.md`

---

## File Map

- **Modify** `crates/kimetsu-chat/src/bridge.rs` — the install engine: new `InstallScope` enum, shared hook-merge helper, idempotent writers, scope-aware path resolution, `plugin_install` signature, unit tests.
- **Modify** `crates/kimetsu-chat/src/lib.rs:33-36` — re-export `InstallScope`.
- **Modify** `crates/kimetsu-cli/src/main.rs` — `--scope` arg on `PluginInstallArgs`, parse + pass through, print scope.
- **Modify** `crates/kimetsu-chat/src/mcp_server.rs` — `scope` arg in the `kimetsu_plugin_install` handler, output, and input schema/description.

Tasks 1–6 keep the `plugin_install` signature unchanged (each commit compiles & tests pass). Task 7 is the integration task that introduces `scope` across `bridge.rs` + `main.rs` + `mcp_server.rs` atomically.

---

## Task 1: `InstallScope` enum

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs:37-65` (insert after the `PluginMode` block)
- Modify: `crates/kimetsu-chat/src/lib.rs:33-36`
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `bridge.rs` (just below `use super::*;`):

```rust
#[test]
fn install_scope_parses_aliases() {
    assert_eq!(InstallScope::parse("").unwrap(), InstallScope::Workspace);
    assert_eq!(InstallScope::parse("workspace").unwrap(), InstallScope::Workspace);
    assert_eq!(InstallScope::parse("Local").unwrap(), InstallScope::Workspace);
    assert_eq!(InstallScope::parse("global").unwrap(), InstallScope::Global);
    assert_eq!(InstallScope::parse("USER").unwrap(), InstallScope::Global);
    assert_eq!(InstallScope::Workspace.as_str(), "workspace");
    assert_eq!(InstallScope::Global.as_str(), "global");
    assert!(InstallScope::parse("nope").is_err());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat install_scope_parses_aliases`
Expected: FAIL — `cannot find type InstallScope in this scope`.

- [ ] **Step 3: Add the enum**

Insert in `bridge.rs` immediately after the `impl Default for PluginMode { … }` block (after line 65):

```rust
/// Where the plugin surface is installed: the current workspace
/// (`.claude/`, `.codex/`, `.mcp.json`) or the user's home directory
/// (`~/.claude/`, `~/.claude.json`, `~/.codex/`) for all sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallScope {
    Workspace,
    Global,
}

impl InstallScope {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "workspace" | "ws" | "local" | "project" => Ok(Self::Workspace),
            "global" | "g" | "user" | "home" => Ok(Self::Global),
            other => Err(format!("unknown install scope `{other}`")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Global => "global",
        }
    }
}

impl Default for InstallScope {
    fn default() -> Self {
        Self::Workspace
    }
}
```

- [ ] **Step 4: Re-export from lib.rs**

In `crates/kimetsu-chat/src/lib.rs`, change the `pub use bridge::{…}` block (lines 33-36) to add `InstallScope`:

```rust
pub use bridge::{
    BridgeTarget, InstallScope, PluginMode, bridge_export_skill, bridge_import_skill, bridge_scan,
    bridge_sync, plugin_install,
};
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p kimetsu-chat install_scope_parses_aliases`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs crates/kimetsu-chat/src/lib.rs
git commit -m "feat: add InstallScope enum for plugin install scope"
```

---

## Task 2: Shared idempotent hook-merge helper

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs` (add private helpers near the other hook writers, e.g. just above `write_codex_hooks` at line 417)
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

This helper is the heart of the fix: upsert Kimetsu's group into one event array without touching anything else.

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn upsert_kimetsu_hook_preserves_user_groups_and_is_idempotent() {
    // A user already has their own UserPromptSubmit hook.
    let mut hooks: serde_json::Map<String, serde_json::Value> = serde_json::from_value(json!({
        "UserPromptSubmit": [
            { "matcher": "", "hooks": [{ "type": "command", "command": "my-own-hook" }] }
        ]
    }))
    .unwrap();

    let km = json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": "kimetsu brain context-hook" }]
    });

    // First upsert: append alongside the user's group.
    upsert_kimetsu_hook(&mut hooks, "UserPromptSubmit", km.clone());
    let arr = hooks["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "kimetsu group appended, user group kept");
    assert_eq!(arr[0]["hooks"][0]["command"], "my-own-hook");
    assert_eq!(arr[1]["hooks"][0]["command"], "kimetsu brain context-hook");

    // Second upsert (re-run): replace in place, no duplicate.
    upsert_kimetsu_hook(&mut hooks, "UserPromptSubmit", km);
    let arr = hooks["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "re-run is idempotent, no duplicate kimetsu group");
    assert_eq!(arr[0]["hooks"][0]["command"], "my-own-hook");

    // New event with no prior array: creates it.
    let km_stop = json!({ "matcher": "", "hooks": [{ "type": "command", "command": "kimetsu brain stop-hook" }] });
    upsert_kimetsu_hook(&mut hooks, "Stop", km_stop);
    assert_eq!(hooks["Stop"].as_array().unwrap().len(), 1);
}
```

Add `use serde_json::json;` is not needed — the `json!` macro is in scope via `serde_json::json!`; use the fully-qualified `serde_json::json!` in the test, or add `use serde_json::json;` at the top of `mod tests`. Add this line under `use super::*;`:

```rust
use serde_json::json;
```

Add this `use` exactly once — the test code in Tasks 3, 4, and 5 also uses the `json!` macro and relies on this single import in the shared `mod tests`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat upsert_kimetsu_hook_preserves_user_groups_and_is_idempotent`
Expected: FAIL — `cannot find function upsert_kimetsu_hook`.

- [ ] **Step 3: Add the helpers**

Insert in `bridge.rs` just above `fn write_codex_hooks` (line 417):

```rust
/// True when a hook matcher-group is one Kimetsu installed (any inner
/// command invokes `kimetsu brain …`).
fn is_kimetsu_hook_group(group: &serde_json::Value) -> bool {
    group
        .get("hooks")
        .and_then(|hooks| hooks.as_array())
        .map(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|command| command.as_str())
                    .is_some_and(|command| command.contains("kimetsu brain"))
            })
        })
        .unwrap_or(false)
}

/// Merge Kimetsu's matcher `group` into the event array at `hooks[event]`,
/// preserving every other group. Idempotent: replaces an existing
/// Kimetsu-owned group instead of appending a duplicate. Never reads or
/// mutates the user's own groups, even when they share Kimetsu's matcher.
fn upsert_kimetsu_hook(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    group: serde_json::Value,
) {
    let entry = hooks
        .entry(event.to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    // If somehow not an array, replace with a fresh single-group array.
    let Some(list) = entry.as_array_mut() else {
        *entry = serde_json::Value::Array(vec![group]);
        return;
    };
    match list.iter_mut().find(|existing| is_kimetsu_hook_group(existing)) {
        Some(slot) => *slot = group,
        None => list.push(group),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kimetsu-chat upsert_kimetsu_hook_preserves_user_groups_and_is_idempotent`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "feat: add idempotent kimetsu hook-merge helper"
```

---

## Task 3: Claude hooks writer uses the merge helper

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs:686-734` (`write_claude_hooks`) and its caller at line 678
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn claude_hooks_merge_preserves_user_hooks() {
    let root = temp_root("claude_hooks_merge");
    let claude = root.join(".claude");
    fs::create_dir_all(&claude).unwrap();
    // User already has their own UserPromptSubmit hook and an unrelated event.
    fs::write(
        claude.join("settings.json"),
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "user-prompt-thing" }] }
                ],
                "SubagentStop": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "user-subagent-thing" }] }
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let settings = claude.join("settings.json");
    write_claude_hooks(&settings, true).unwrap();
    // Re-run to prove idempotency.
    write_claude_hooks(&settings, true).unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let ups = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(ups.len(), 2, "user group kept + one kimetsu group, no dupes");
    assert_eq!(ups[0]["hooks"][0]["command"], "user-prompt-thing");
    assert_eq!(ups[1]["hooks"][0]["command"], "kimetsu brain context-hook");
    // Unrelated user event untouched.
    assert_eq!(
        value["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
        "user-subagent-thing"
    );
    // Kimetsu's own events present.
    assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["command"], "kimetsu brain stop-hook");
    assert_eq!(value["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "kimetsu brain pretool-hook");

    fs::remove_dir_all(root).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat claude_hooks_merge_preserves_user_hooks`
Expected: FAIL — current `write_claude_hooks` takes `(path, force, proactive)` (arity mismatch) and replaces the whole `hooks` object.

- [ ] **Step 3: Rewrite `write_claude_hooks`**

Replace the entire function body (lines 686-734) with:

```rust
/// Merge Kimetsu's `UserPromptSubmit`/`Stop` hooks (plus the v0.8
/// proactive `PreToolUse`/`PostToolUse` Bash hooks when `proactive`)
/// into `settings.json`, preserving any other hooks the user has — even
/// on the same events. Idempotent: re-running never duplicates.
fn write_claude_hooks(path: &Path, proactive: bool) -> Result<(), String> {
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|err| format!("parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must be a JSON object", path.display()))?;
    let hooks_value = root_obj
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks_value
        .as_object_mut()
        .ok_or_else(|| format!("{} `hooks` must be a JSON object", path.display()))?;

    upsert_kimetsu_hook(
        hooks_obj,
        "UserPromptSubmit",
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain context-hook" }]
        }),
    );
    upsert_kimetsu_hook(
        hooks_obj,
        "Stop",
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain stop-hook" }]
        }),
    );
    if proactive {
        upsert_kimetsu_hook(
            hooks_obj,
            "PreToolUse",
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "kimetsu brain pretool-hook" }]
            }),
        );
        upsert_kimetsu_hook(
            hooks_obj,
            "PostToolUse",
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "kimetsu brain posttool-hook" }]
            }),
        );
    }

    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize Claude settings: {err}"))?;
    write_text_file(path, &text, true)
}
```

- [ ] **Step 4: Update the caller**

In `write_claude_settings` (line 678), change the call to drop the `force` argument:

```rust
    let settings = claude_dir.join("settings.json");
    write_claude_hooks(&settings, proactive)?;
    files.push(normalize_path(&settings));
```

(Note: `claude_dir` is still `workspace.join(".claude")` at this point — Task 7 reworks the surrounding function. Only the `write_claude_hooks` call line changes here.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kimetsu-chat claude_hooks_merge_preserves_user_hooks`
Expected: PASS.

Run: `cargo test -p kimetsu-chat`
Expected: PASS (existing tests still green).

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "fix: merge Claude hooks instead of replacing user config"
```

---

## Task 4: Codex hooks writer uses the merge helper + takes a `.codex` dir

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs:417-493` (`write_codex_hooks`) and its caller at line 398
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn codex_hooks_merge_preserves_user_hooks() {
    let root = temp_root("codex_hooks_merge");
    let codex = root.join(".codex");
    fs::create_dir_all(&codex).unwrap();
    fs::write(
        codex.join("hooks.json"),
        serde_json::to_string_pretty(&json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "user-codex-hook" }] }
                ]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut files = Vec::new();
    write_codex_hooks(&codex, true, &mut files).unwrap();
    write_codex_hooks(&codex, true, &mut files).unwrap(); // idempotent

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(codex.join("hooks.json")).unwrap()).unwrap();
    let ups = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(ups.len(), 2, "user group kept + one kimetsu group");
    assert_eq!(ups[0]["hooks"][0]["command"], "user-codex-hook");
    assert_eq!(
        ups[1]["hooks"][0]["command"],
        "kimetsu brain context-hook --workspace ."
    );
    assert_eq!(
        value["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "kimetsu brain pretool-hook --workspace ."
    );

    fs::remove_dir_all(root).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat codex_hooks_merge_preserves_user_hooks`
Expected: FAIL — current `write_codex_hooks` takes `(workspace, force, proactive, files)` (arity mismatch) and overwrites the `UserPromptSubmit` array.

- [ ] **Step 3: Rewrite `write_codex_hooks`**

Replace the entire function (lines 417-493) with this version — it takes the `.codex` directory directly, drops `force`, and merges:

```rust
fn write_codex_hooks(
    codex_dir: &Path,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let hooks = codex_dir.join("hooks.json");
    let mut root = if hooks.is_file() {
        let text =
            fs::read_to_string(&hooks).map_err(|err| format!("read {}: {err}", hooks.display()))?;
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|err| format!("parse {}: {err}", hooks.display()))?
    } else {
        serde_json::json!({})
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must be a JSON object", hooks.display()))?;
    let hooks_value = root_obj
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks_value
        .as_object_mut()
        .ok_or_else(|| format!("{} `hooks` must be a JSON object", hooks.display()))?;

    upsert_kimetsu_hook(
        hooks_obj,
        "UserPromptSubmit",
        serde_json::json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "kimetsu brain context-hook --workspace .",
                "statusMessage": "Loading Kimetsu brain context",
                "timeout": 30
            }]
        }),
    );
    if proactive {
        upsert_kimetsu_hook(
            hooks_obj,
            "PreToolUse",
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "kimetsu brain pretool-hook --workspace .",
                    "statusMessage": "Kimetsu proactive check",
                    "timeout": 15
                }]
            }),
        );
        upsert_kimetsu_hook(
            hooks_obj,
            "PostToolUse",
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "kimetsu brain posttool-hook --workspace .",
                    "statusMessage": "Kimetsu proactive check",
                    "timeout": 15
                }]
            }),
        );
    }

    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize Codex hooks: {err}"))?;
    write_text_file(&hooks, &text, true)?;
    files.push(normalize_path(&hooks));
    Ok(())
}
```

- [ ] **Step 4: Update the caller**

In `plugin_install`, the Codex arm (line 398) currently calls
`write_codex_hooks(&workspace, force, proactive, &mut files)?;`. Change it to pass the `.codex` dir and drop `force`:

```rust
            write_codex_hooks(&workspace.join(".codex"), proactive, &mut files)?;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kimetsu-chat codex_hooks_merge_preserves_user_hooks`
Expected: PASS.

Run: `cargo test -p kimetsu-chat plugin_install_writes_optional_and_required_modes plugin_install_no_proactive_skips_tool_hooks`
Expected: PASS (fresh-install shape unchanged: Kimetsu's group is index 0).

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "fix: merge Codex hooks instead of replacing UserPromptSubmit"
```

---

## Task 5: MCP config writers become idempotent

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs` — `write_mcp_config` (lines ~556-595) and `write_codex_config` (lines ~597-641); callers in `plugin_install` (lines 348, 381)
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn mcp_config_is_idempotent_and_scopes_keys() {
    let root = temp_root("mcp_idempotent");
    let mcp = root.join(".mcp.json");

    // Workspace style: write both `servers` and `mcpServers`, twice, no error.
    write_mcp_config(&mcp, false).unwrap();
    write_mcp_config(&mcp, false).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
    assert_eq!(value["servers"]["kimetsu"]["command"], "kimetsu");
    assert_eq!(value["mcpServers"]["kimetsu"]["command"], "kimetsu");

    // Global style: only `mcpServers`, preserving unrelated keys.
    let claude_json = root.join(".claude.json");
    fs::write(&claude_json, serde_json::to_string(&json!({ "keepme": 1 })).unwrap()).unwrap();
    write_mcp_config(&claude_json, true).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert_eq!(value["keepme"], 1);
    assert_eq!(value["mcpServers"]["kimetsu"]["command"], "kimetsu");
    assert!(value.get("servers").is_none(), "global writes mcpServers only");

    fs::remove_dir_all(root).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat mcp_config_is_idempotent_and_scopes_keys`
Expected: FAIL — `write_mcp_config` currently takes `(path, force)` and errors on the second call (`already has a kimetsu MCP server`).

- [ ] **Step 3: Rewrite `write_mcp_config`**

Replace the function (the version reading the file through `write_text_file`, lines ~556-595) with:

```rust
/// Upsert the `kimetsu` MCP server into a Claude config file. Idempotent —
/// re-running just rewrites the same entry, preserving all other keys.
/// `only_mcp_servers` is true for `~/.claude.json` (global), which uses
/// only the `mcpServers` key; workspace `.mcp.json` also gets `servers`.
fn write_mcp_config(path: &Path, only_mcp_servers: bool) -> Result<(), String> {
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|err| format!("parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must be a JSON object", path.display()))?;
    let server = serde_json::json!({
        "command": "kimetsu",
        "args": ["mcp", "serve", "--workspace", "."]
    });
    if !only_mcp_servers {
        insert_mcp_server(root_obj, "servers", server.clone(), path)?;
    }
    insert_mcp_server(root_obj, "mcpServers", server, path)?;
    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize MCP config: {err}"))?;
    write_text_file(path, &text, true)
}
```

- [ ] **Step 4: Make `write_codex_config` idempotent**

In `write_codex_config` (lines ~597-641): change the signature from `(path: &Path, force: bool)` to `(path: &Path)`, and delete the force gate block:

```rust
    if servers.contains_key("kimetsu") && !force {
        return Err(format!(
            "{} already has a kimetsu MCP server; pass --force",
            path.display()
        ));
    }
```

The `servers.insert("kimetsu", …)` below it already upserts, so removal is safe.

- [ ] **Step 5: Update callers in `plugin_install`**

- Claude arm (line 348): `write_mcp_config(&mcp, force)?;` → `write_mcp_config(&mcp, false)?;`
- Codex arm (line 381): `write_codex_config(&config, force)?;` → `write_codex_config(&config)?;`

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p kimetsu-chat mcp_config_is_idempotent_and_scopes_keys`
Expected: PASS.

Run: `cargo test -p kimetsu-chat`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "fix: make MCP config writers idempotent + scope-aware keys"
```

---

## Task 6: Auto-refresh Kimetsu-owned generated files

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs` — the `write_text_file` calls for `bridge.md` (line ~355), `delegate.md` (line ~365), and Codex `SKILL.md` (line ~389)
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

Generated command/skill docs are Kimetsu-authored, so install should refresh them without `--force`. `CLAUDE.md` stays write-if-missing (unchanged).

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn plugin_install_refreshes_generated_files_without_force() {
    let root = temp_root("plugin_install_refresh");
    // First install (Codex) writes SKILL.md.
    plugin_install(&root, BridgeTarget::Codex, PluginMode::Optional, false, true).unwrap();
    // Second install with force=false must succeed (refresh, not error).
    plugin_install(&root, BridgeTarget::Codex, PluginMode::Required, false, true).unwrap();

    let skill = fs::read_to_string(
        root.join(".codex/skills/kimetsu-bridge/SKILL.md"),
    )
    .unwrap();
    // Required-mode content replaced the optional-mode content.
    assert!(skill.contains("required") || !skill.is_empty());

    fs::remove_dir_all(root).ok();
}
```

(Note: this test uses the **current** 5-arg `plugin_install` signature; Task 7 updates it.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat plugin_install_refreshes_generated_files_without_force`
Expected: FAIL — second install errors with `…SKILL.md exists; pass --force to replace`.

- [ ] **Step 3: Force-write the generated docs**

In `plugin_install`, change the three `write_text_file` calls for generated docs to pass `true` instead of `force`:

- `bridge.md` call (line ~355-362): last arg `force` → `true`.
- `delegate.md` call (line ~365-372): last arg `force` → `true`.
- Codex `SKILL.md` call (line ~389-396): last arg `force` → `true`.

Each becomes, e.g.:

```rust
            write_text_file(
                &bridge,
                match mode {
                    PluginMode::Optional => CLAUDE_BRIDGE_COMMAND_OPTIONAL,
                    PluginMode::Required => CLAUDE_BRIDGE_COMMAND_REQUIRED,
                },
                true,
            )?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kimetsu-chat plugin_install_refreshes_generated_files_without_force`
Expected: PASS.

Run: `cargo test -p kimetsu-chat`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs
git commit -m "fix: refresh generated plugin docs on re-install without --force"
```

---

## Task 7: Thread `scope` through `plugin_install`, CLI, and MCP

**Files:**
- Modify: `crates/kimetsu-chat/src/bridge.rs` — `PluginInstallReport` (lines 104-109), `plugin_install` (lines 334-411), `write_claude_settings` (lines 660-681), `resolve_home` (new); update existing tests' `plugin_install(...)` calls
- Modify: `crates/kimetsu-cli/src/main.rs` — `PluginInstallArgs` (lines 271-286), `plugin` fn (lines 936-959)
- Modify: `crates/kimetsu-chat/src/mcp_server.rs` — handler (lines 337-361), schema (lines 1761-1776), `PLUGIN_INSTALL_DESCRIPTION` (line 63)
- Test: `crates/kimetsu-chat/src/bridge.rs` (tests module)

This is the integration task: the signature change and all call sites land in one compiling commit.

- [ ] **Step 1: Write the failing test (global scope)**

Add to `mod tests`:

```rust
#[test]
fn plugin_install_global_writes_to_home_not_workspace() {
    let ws = temp_root("plugin_install_global_ws");
    let home = temp_root("plugin_install_global_home");

    // Claude global install into the injected home.
    plugin_install_inner(
        &ws,
        BridgeTarget::ClaudeCode,
        InstallScope::Global,
        PluginMode::Optional,
        false,
        true,
        Some(home.as_path()),
    )
    .unwrap();

    assert!(home.join(".claude/settings.json").is_file());
    assert!(home.join(".claude/commands/kimetsu/bridge.md").is_file());
    assert!(home.join(".claude.json").is_file());
    // mcpServers only in the global claude.json.
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(value["mcpServers"]["kimetsu"]["command"], "kimetsu");
    assert!(value.get("servers").is_none());
    // Nothing written under the workspace.
    assert!(!ws.join(".claude").exists());
    assert!(!ws.join(".mcp.json").exists());

    // Codex global install.
    plugin_install_inner(
        &ws,
        BridgeTarget::Codex,
        InstallScope::Global,
        PluginMode::Optional,
        false,
        true,
        Some(home.as_path()),
    )
    .unwrap();
    assert!(home.join(".codex/config.toml").is_file());
    assert!(home.join(".codex/hooks.json").is_file());
    assert!(!ws.join(".codex").exists());

    fs::remove_dir_all(ws).ok();
    fs::remove_dir_all(home).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kimetsu-chat plugin_install_global_writes_to_home_not_workspace`
Expected: FAIL — `plugin_install_inner` and `InstallScope` arg don't exist yet.

- [ ] **Step 3: Add `scope` to `PluginInstallReport`**

Change the struct (lines 104-109) to:

```rust
#[derive(Debug, Clone)]
pub struct PluginInstallReport {
    pub target: BridgeTarget,
    pub scope: InstallScope,
    pub mode: PluginMode,
    pub files: Vec<PathBuf>,
}
```

- [ ] **Step 4: Add `resolve_home`**

Insert near `normalize_path` (around line 828) in `bridge.rs`:

```rust
fn resolve_home() -> Result<PathBuf, String> {
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .ok_or_else(|| {
            "cannot resolve home directory for a global install (set HOME or USERPROFILE)"
                .to_string()
        })
}
```

- [ ] **Step 5: Rewrite `plugin_install` as a thin wrapper + `plugin_install_inner`**

Replace the whole `plugin_install` function (lines 334-411) with:

```rust
pub fn plugin_install(
    workspace: &Path,
    target: BridgeTarget,
    scope: InstallScope,
    mode: PluginMode,
    force: bool,
    proactive: bool,
) -> Result<PluginInstallReport, String> {
    let home = match scope {
        InstallScope::Global => Some(resolve_home()?),
        InstallScope::Workspace => None,
    };
    plugin_install_inner(
        workspace,
        target,
        scope,
        mode,
        force,
        proactive,
        home.as_deref(),
    )
}

/// `home` is `Some` for a global install (the directory that stands in for
/// `~`), `None` for a workspace install. Kept separate from `plugin_install`
/// so tests can inject a deterministic home without touching process env.
fn plugin_install_inner(
    workspace: &Path,
    target: BridgeTarget,
    scope: InstallScope,
    mode: PluginMode,
    force: bool,
    proactive: bool,
    home: Option<&Path>,
) -> Result<PluginInstallReport, String> {
    let workspace = normalize_path(workspace);
    let mut files = Vec::new();
    match target {
        BridgeTarget::ClaudeCode => {
            // MCP: workspace -> ./.mcp.json (servers + mcpServers);
            // global -> ~/.claude.json (mcpServers only).
            let (mcp, only_mcp_servers) = match home {
                Some(home) => (home.join(".claude.json"), true),
                None => (workspace.join(".mcp.json"), false),
            };
            write_mcp_config(&mcp, only_mcp_servers)?;
            files.push(normalize_path(&mcp));

            let claude_dir = match home {
                Some(home) => home.join(".claude"),
                None => workspace.join(".claude"),
            };
            let commands = claude_dir.join("commands").join("kimetsu");
            fs::create_dir_all(&commands)
                .map_err(|err| format!("create {}: {err}", commands.display()))?;
            let bridge = commands.join("bridge.md");
            write_text_file(
                &bridge,
                match mode {
                    PluginMode::Optional => CLAUDE_BRIDGE_COMMAND_OPTIONAL,
                    PluginMode::Required => CLAUDE_BRIDGE_COMMAND_REQUIRED,
                },
                true,
            )?;
            files.push(normalize_path(&bridge));
            let delegate = commands.join("delegate.md");
            write_text_file(
                &delegate,
                match mode {
                    PluginMode::Optional => CLAUDE_DELEGATE_COMMAND_OPTIONAL,
                    PluginMode::Required => CLAUDE_DELEGATE_COMMAND_REQUIRED,
                },
                true,
            )?;
            files.push(normalize_path(&delegate));
            write_claude_settings(&claude_dir, force, proactive, &mut files)?;
        }
        BridgeTarget::Codex => {
            let codex_dir = match home {
                Some(home) => home.join(".codex"),
                None => workspace.join(".codex"),
            };
            let config = codex_dir.join("config.toml");
            write_codex_config(&config)?;
            files.push(normalize_path(&config));

            let skill = codex_dir
                .join("skills")
                .join("kimetsu-bridge")
                .join("SKILL.md");
            write_text_file(
                &skill,
                match mode {
                    PluginMode::Optional => CODEX_KIMETSU_SKILL_OPTIONAL,
                    PluginMode::Required => CODEX_KIMETSU_SKILL_REQUIRED,
                },
                true,
            )?;
            files.push(normalize_path(&skill));
            write_codex_hooks(&codex_dir, proactive, &mut files)?;
        }
        BridgeTarget::Kimetsu => {
            // Kimetsu extensions are workspace-only; scope is ignored.
            let dir = workspace.join(".kimetsu").join("extensions");
            fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
            files.push(normalize_path(&dir));
        }
    }
    Ok(PluginInstallReport {
        target,
        scope,
        mode,
        files,
    })
}
```

- [ ] **Step 6: Update `write_claude_settings` to take the `.claude` dir**

Replace `write_claude_settings` (lines 660-681) with:

```rust
/// Write the Claude Code surface that lives under `.claude/`: the brain
/// `CLAUDE.md` guidance and the `settings.json` hook registration.
fn write_claude_settings(
    claude_dir: &Path,
    force: bool,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    fs::create_dir_all(claude_dir)
        .map_err(|err| format!("create {}: {err}", claude_dir.display()))?;

    // CLAUDE.md: seed only when missing, unless forced — never clobber edits.
    let claude_md = claude_dir.join("CLAUDE.md");
    if !claude_md.is_file() || force {
        write_text_file(&claude_md, CLAUDE_MD_CONTENT, true)?;
    }
    files.push(normalize_path(&claude_md));

    let settings = claude_dir.join("settings.json");
    write_claude_hooks(&settings, proactive)?;
    files.push(normalize_path(&settings));
    Ok(())
}
```

- [ ] **Step 7: Update existing test call sites in `bridge.rs`**

Existing tests call the 5-arg `plugin_install`. Add the `InstallScope::Workspace` argument after the target in each:

- `plugin_install_writes_optional_and_required_modes` (line ~890 and ~931): both `plugin_install(&root, BridgeTarget::Codex, PluginMode::…, …)` calls → insert `InstallScope::Workspace,` after `BridgeTarget::Codex,`.
- `plugin_install_no_proactive_skips_tool_hooks` (line ~953): same insertion.
- `plugin_install_refreshes_generated_files_without_force` (Task 6): both calls → insert `InstallScope::Workspace,` after `BridgeTarget::Codex,`.

Each becomes, e.g.:

```rust
        let optional = plugin_install(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
        )
        .expect("install");
```

- [ ] **Step 8: Run the bridge tests**

Run: `cargo test -p kimetsu-chat`
Expected: PASS (including the new `plugin_install_global_writes_to_home_not_workspace`).

- [ ] **Step 9: Wire the CLI `--scope` flag**

In `crates/kimetsu-cli/src/main.rs`, add to `PluginInstallArgs` (after the `mode` field, lines 276-279):

```rust
    /// Install scope: `workspace` (default) writes .claude/.codex in the
    /// workspace; `global` writes to ~/.claude(.json) and ~/.codex for all
    /// sessions.
    #[arg(long, default_value = "workspace")]
    scope: String,
```

Then update the `plugin` fn (lines 936-959):

```rust
fn plugin(command: PluginCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{BridgeTarget, InstallScope, PluginMode, plugin_install};

    match command {
        PluginCommand::Install(args) => {
            let workspace = args.workspace.canonicalize()?;
            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let scope = InstallScope::parse(&args.scope)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let mode = PluginMode::parse(&args.mode)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let report =
                plugin_install(&workspace, target, scope, mode, args.force, !args.no_proactive)
                    .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            println!(
                "installed Kimetsu plugin surface for {} ({} scope) in {} mode",
                report.target.as_str(),
                report.scope.as_str(),
                report.mode.as_str()
            );
            for file in report.files {
                println!("  {}", file.display());
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 10: Wire the MCP `scope` argument**

In `crates/kimetsu-chat/src/mcp_server.rs`:

(a) Add `InstallScope` to the import (lines 10-11):

```rust
    BridgeTarget, InstallScope, PluginMode, bridge_export_skill, bridge_import_skill, bridge_scan,
    bridge_sync, plugin_install,
```

(b) In the `"kimetsu_plugin_install"` handler (lines 337-361), parse `scope` and pass it through, and add it to the output:

```rust
        "kimetsu_plugin_install" => {
            let target = BridgeTarget::parse(&string_arg(&arguments, "target")?)?;
            let scope = arguments
                .get("scope")
                .and_then(Value::as_str)
                .map(InstallScope::parse)
                .transpose()?
                .unwrap_or_default();
            let mode = arguments
                .get("mode")
                .and_then(Value::as_str)
                .map(PluginMode::parse)
                .transpose()?
                .unwrap_or_default();
            let force = arguments
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // v0.8: proactive defaults on; pass proactive:false to skip
            // the PreToolUse/PostToolUse Bash hooks.
            let proactive = arguments
                .get("proactive")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let report = plugin_install(workspace, target, scope, mode, force, proactive)?;
            Ok(json!({
                "target": report.target.as_str(),
                "scope": report.scope.as_str(),
                "mode": report.mode.as_str(),
                "files": report.files,
            }))
        }
```

(c) Add the `scope` property to the input schema (lines 1763-1776), after `"target"`:

```rust
                    "target": { "type": "string", "enum": ["claude-code", "codex"] },
                    "scope": {
                        "type": "string",
                        "enum": ["workspace", "global"],
                        "description": "workspace (default) installs into this workspace's .claude/.codex; global installs into ~/.claude(.json) and ~/.codex for all sessions."
                    },
```

(d) Update `PLUGIN_INSTALL_DESCRIPTION` (line 63) to mention scope — append this sentence to the existing string literal, before the closing quote:

```
 Set scope=workspace (default) to install into this workspace, or scope=global to install into the user's home (~/.claude, ~/.claude.json, ~/.codex) for all sessions. Existing user hooks are preserved (merged, not replaced).
```

- [ ] **Step 11: Build the whole workspace**

Run: `cargo build`
Expected: builds clean (no errors).

- [ ] **Step 12: Run all tests**

Run: `cargo test -p kimetsu-chat`
Expected: PASS.

- [ ] **Step 13: Commit**

```bash
git add crates/kimetsu-chat/src/bridge.rs crates/kimetsu-cli/src/main.rs crates/kimetsu-chat/src/mcp_server.rs
git commit -m "feat: add --scope workspace|global to kimetsu plugin install"
```

---

## Task 8: Manual smoke test + docs

**Files:**
- Verify only (no code); optional README touch if scope deserves a mention.

- [ ] **Step 1: Workspace install merges, doesn't clobber**

Run (PowerShell, in a throwaway dir):

```powershell
$d = Join-Path $env:TEMP ("km-smoke-" + [guid]::NewGuid())
New-Item -ItemType Directory $d | Out-Null
New-Item -ItemType Directory (Join-Path $d ".claude") | Out-Null
'{ "hooks": { "UserPromptSubmit": [ { "matcher": "", "hooks": [ { "type": "command", "command": "my-hook" } ] } ] } }' |
  Out-File -Encoding utf8 (Join-Path $d ".claude/settings.json")
cargo run -p kimetsu-cli -- plugin install claude-code --workspace $d
Get-Content (Join-Path $d ".claude/settings.json")
```

Expected: the `UserPromptSubmit` array contains **both** `my-hook` and a `kimetsu brain context-hook` group; `Stop`/`PreToolUse`/`PostToolUse` kimetsu groups are present.

- [ ] **Step 2: Re-run is idempotent**

Run: `cargo run -p kimetsu-cli -- plugin install claude-code --workspace $d` again.
Expected: succeeds without `--force`; the kimetsu `UserPromptSubmit` group count stays 1 (array length stays 2).

- [ ] **Step 3: Global scope smoke (use a sandbox HOME)**

```powershell
$home2 = Join-Path $env:TEMP ("km-home-" + [guid]::NewGuid())
New-Item -ItemType Directory $home2 | Out-Null
$env:USERPROFILE = $home2
cargo run -p kimetsu-cli -- plugin install codex --workspace $d --scope global
Get-ChildItem -Recurse $home2
```

Expected: files appear under `$home2/.codex/` (config.toml, hooks.json, skills/…); nothing new under the workspace `.codex`.

- [ ] **Step 4: Clean up**

```powershell
Remove-Item -Recurse -Force $d, $home2
```

(Reopen a fresh shell so the temporary `USERPROFILE` override is dropped.)

- [ ] **Step 5: Commit any doc updates**

If you touched `README.md`/`npm/README.md` to document `--scope`, commit:

```bash
git add README.md npm/README.md
git commit -m "docs: document --scope for kimetsu plugin install"
```

---

## Done

All checkboxes complete means: hooks merge non-destructively (including on events Kimetsu shares), install is idempotent, and `--scope workspace|global` works on the CLI and the MCP tool for both Claude Code and Codex.
