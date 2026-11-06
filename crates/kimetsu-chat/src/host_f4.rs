//! ADR 0001 F4 host edge.
//!
//! Claude Code, Codex, and Cursor get the documented stdio MCP entry.
//! Grok Bot has no settings file; the installer writes an operator snippet
//! the person pastes into chat. None of this opens `brain.db`.

use std::path::{Path, PathBuf};

/// Local stdio server every file-based host starts. Args match `kimetsu mcp serve`.
pub(crate) const STDIO_COMMAND: &str = "kimetsu";
pub(crate) const STDIO_ARGS: &[&str] = &["mcp", "serve", "--workspace", "."];

/// One operator workflow for every F4 host and for the MCP `instructions` field.
///
/// Avoids the phrases the initialize contract rejects (`do not call`, `non-trivial`).
pub(crate) const OPERATOR_WORKFLOW: &str = "\
Mount grants for this stdio MCP session are explicit: read `repo` and read `user`. \
Write is `repo` only, and only while write tools are enabled. \
`kimetsu_brain_record` stores the lesson on `repo`. It does not store `user`, `org`, or `skill`. \
`kimetsu_brain_context` returns capsules from the read grants. \
Retrieval uses stored evidence and skips any generative model. \
Warm start, when present, is that same evidence digest. \
After a retrieved memory changes the work, call `kimetsu_brain_cite` with `exposure_id` from the context response and `memory_id` from the capsule. \
`outcome` is `helped`, `noop`, or `harmed`. Omitting `outcome` records `noop`. \
Only `helped` may raise usefulness. `harmed` removes the memory from injection. \
A citation records reliance, not verification.\n";

pub(crate) fn stdio_command_line() -> String {
    let mut line = STDIO_COMMAND.to_string();
    for arg in STDIO_ARGS {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

pub(crate) fn grok_bot_snippet_path(workspace: &Path, home: Option<&Path>) -> PathBuf {
    let base = home.unwrap_or(workspace);
    base.join(".kimetsu").join("hosts").join("grok-bot.md")
}

/// Operator snippet. `remote` is `(endpoint, authorization header value)`.
pub(crate) fn grok_bot_snippet(mode: &str, remote: Option<(&str, &str)>) -> String {
    let stdio_line = stdio_command_line();
    let remote_section = match remote {
        Some((endpoint, authorization)) => format!(
            "Ask the Bot to add this HTTPS MCP URL. A private network or a private certificate authority asks for approval first.\n\n```\n{endpoint}\nAuthorization: {authorization}\n```\n"
        ),
        None => "This install names no remote URL. Ask the Bot to add the local command on its computer. For an HTTP endpoint, run `kimetsu plugin install grok-bot --remote <base-url>`.\n".to_string(),
    };
    format!(
        "\
# Kimetsu on Grok Bot

This file is an operator snippet, not a Grok Bot config. Grok Bot publishes no settings file for a custom MCP server. Ask the Bot to add a server that is not in the catalog, by URL or by a command that runs on its computer (https://docs.x.ai/grok-bot/computer-and-apps, read 2026-11-06). A command-started server is available to your other Bots. This installer does not draw a Connect card and does not write Grok Build's `~/.grok/config.toml` (https://docs.x.ai/build/features/mcp-servers).

Mode: {mode}.

## Local command

```
{stdio_line}
```

## Remote URL

{remote_section}
## Operator workflow

{OPERATOR_WORKFLOW}"
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use kimetsu_brain::project;
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use serde_json::{Value, json};

    use crate::bridge::{
        BridgeTarget, InstallScope, PluginMode, RemoteInstall, WiringState, plugin_install,
        plugin_install_remote, plugin_status, plugin_uninstall,
    };
    use crate::mcp_server::dispatch;
    use crate::skills::SkillConfig;

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kimetsu_f4_{label}_{nanos}"));
        fs::create_dir_all(&path).expect("root");
        kimetsu_core::paths::git_init_boundary(&path);
        path
    }

    fn mcp_call(workspace: &Path, name: &str, arguments: Value) -> Value {
        let envelope = dispatch(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            workspace,
            &SkillConfig::default(),
            None,
        )
        .unwrap_or_else(|err| panic!("tools/call {name}: {err}"));
        let text = envelope["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("missing content text: {envelope}"));
        serde_json::from_str(text).unwrap_or_else(|err| panic!("tool json: {err}: {text}"))
    }

    fn stdio_entry(value: &Value) -> &Value {
        &value["mcpServers"]["kimetsu"]
    }

    #[test]
    fn four_hosts_generate_stdio_mcp_and_drive_context_record_cite() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = temp_root("workflow");
            project::init_project(&root, false).expect("init");
            project::add_memory(
                &root,
                MemoryScope::Org,
                MemoryKind::Fact,
                "lanternwick ORG_SECRET_LEAK policy",
            )
            .expect("org");
            project::add_memory(
                &root,
                MemoryScope::Skill,
                MemoryKind::Fact,
                "lanternwick SKILL_SECRET_LEAK lesson",
            )
            .expect("skill");

            for target in ["claude-code", "codex", "cursor", "grok-bot"] {
                let parsed = BridgeTarget::parse(target)
                    .unwrap_or_else(|err| panic!("F4 host `{target}` must install: {err}"));
                plugin_install(
                    &root,
                    parsed,
                    InstallScope::Workspace,
                    PluginMode::Optional,
                    false,
                    false,
                )
                .unwrap_or_else(|err| panic!("install {target}: {err}"));
            }

            let claude: Value = serde_json::from_str(
                &fs::read_to_string(root.join(".mcp.json")).expect("claude mcp"),
            )
            .expect("claude json");
            let claude_stdio = stdio_entry(&claude);
            assert_eq!(claude_stdio["type"], "stdio");
            assert_eq!(claude_stdio["command"], "kimetsu");
            assert_eq!(
                claude_stdio["args"],
                json!(["mcp", "serve", "--workspace", "."])
            );

            let codex: toml::Value = toml::from_str(
                &fs::read_to_string(root.join(".codex/config.toml")).expect("codex config"),
            )
            .expect("codex toml");
            assert_eq!(
                codex["mcp_servers"]["kimetsu"]["command"].as_str(),
                Some("kimetsu")
            );
            let codex_args = codex["mcp_servers"]["kimetsu"]["args"].as_array().unwrap();
            assert_eq!(
                codex_args
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["mcp", "serve", "--workspace", "."]
            );

            let cursor: Value = serde_json::from_str(
                &fs::read_to_string(root.join(".cursor/mcp.json")).expect("cursor mcp"),
            )
            .expect("cursor json");
            let cursor_stdio = stdio_entry(&cursor);
            assert_eq!(cursor_stdio["type"], "stdio");
            assert_eq!(cursor_stdio["command"], "kimetsu");
            assert_eq!(
                cursor_stdio["args"],
                json!(["mcp", "serve", "--workspace", "."])
            );

            let grok = fs::read_to_string(root.join(".kimetsu/hosts/grok-bot.md")).expect("grok");
            assert!(
                grok.contains("kimetsu mcp serve --workspace ."),
                "snippet must name the stdio command: {grok}"
            );
            assert!(
                grok.contains("https://docs.x.ai/grok-bot/computer-and-apps"),
                "snippet must cite the Grok Bot computer docs"
            );
            assert!(
                grok.contains("not a Grok Bot config"),
                "snippet must say it is not a config file"
            );
            assert!(
                !grok.contains("\"mcpServers\""),
                "do not invent a Grok Bot mcp.json"
            );
            assert!(
                !grok.contains("[mcp_servers.kimetsu]"),
                "do not invent a Grok Build config.toml entry"
            );

            for guidance in [
                root.join(".claude/commands/kimetsu/bridge.md"),
                root.join(".codex/skills/kimetsu-bridge/SKILL.md"),
                root.join(".cursor/rules/kimetsu-brain/rule.md"),
                root.join(".kimetsu/hosts/grok-bot.md"),
            ] {
                let text = fs::read_to_string(&guidance)
                    .unwrap_or_else(|err| panic!("read {}: {err}", guidance.display()));
                for phrase in [
                    "helped",
                    "noop",
                    "harmed",
                    "read `repo`",
                    "generative model",
                ] {
                    assert!(
                        text.contains(phrase),
                        "{} missing `{phrase}`",
                        guidance.display()
                    );
                }
            }

            let grok_status = plugin_status(&root)
                .into_iter()
                .find(|row| row.host == "grok-bot" && row.scope == "workspace")
                .expect("grok-bot status");
            assert!(
                matches!(grok_status.state, WiringState::Installed),
                "grok-bot status: {:?}",
                grok_status.state
            );

            let init = dispatch(
                "initialize",
                json!({}),
                &root,
                &SkillConfig::default(),
                None,
            )
            .expect("initialize");
            let instructions = init["instructions"].as_str().unwrap();
            for phrase in [
                "helped",
                "noop",
                "harmed",
                "read `repo`",
                "generative model",
            ] {
                assert!(
                    instructions.contains(phrase),
                    "MCP instructions missing `{phrase}`"
                );
            }
            assert!(!instructions.contains("non-trivial"));
            assert!(!instructions.to_ascii_lowercase().contains("do not call"));

            let recorded = mcp_call(
                &root,
                "kimetsu_brain_record",
                json!({
                    "lesson": "lanternwick f4helpedtoken keep the repo mount",
                    "tags": ["f4", "mount"],
                    "level": "org",
                    "scope": "user"
                }),
            );
            assert_eq!(recorded["ok"], true, "{recorded}");
            assert_eq!(recorded["level"], "repo");
            assert_eq!(recorded["grants"]["read"], json!(["repo", "user"]));
            assert_eq!(recorded["grants"]["write"], json!(["repo"]));
            let helped_id = recorded["memory_id"].as_str().unwrap().to_string();

            let noop = mcp_call(
                &root,
                "kimetsu_brain_record",
                json!({
                    "lesson": "lanternwick f4nooptoken leave usefulness alone",
                    "tags": ["f4"]
                }),
            );
            let noop_id = noop["memory_id"].as_str().unwrap().to_string();
            let harmed = mcp_call(
                &root,
                "kimetsu_brain_record",
                json!({
                    "lesson": "lanternwick f4harmedtoken drop this injection",
                    "tags": ["f4"]
                }),
            );
            let harmed_id = harmed["memory_id"].as_str().unwrap().to_string();

            let (_paths, _config, conn) = project::load_project(&root).unwrap();
            let scope: String = conn
                .query_row(
                    "SELECT scope FROM memories WHERE memory_id=?1",
                    [helped_id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(scope, "repo");
            let score = |id: &str| -> f64 {
                conn.query_row(
                    "SELECT usefulness_score FROM memories WHERE memory_id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
            };
            let before_helped = score(&helped_id);
            let before_noop = score(&noop_id);
            let before_harmed = score(&harmed_id);
            drop(conn);

            let context = mcp_call(
                &root,
                "kimetsu_brain_context",
                json!({
                    "query": "lanternwick",
                    "min_score": 0.0,
                    "max_capsules": 8,
                    "budget_tokens": 8000,
                    "include_ambient": false
                }),
            );
            let payload = context.to_string();
            assert!(
                payload.contains("f4helpedtoken"),
                "recorded repo lesson missing: {payload}"
            );
            assert!(
                !payload.contains("ORG_SECRET_LEAK"),
                "org text leaked: {payload}"
            );
            assert!(
                !payload.contains("SKILL_SECRET_LEAK"),
                "skill text leaked: {payload}"
            );
            assert!(
                payload.contains("f4nooptoken") && payload.contains("f4harmedtoken"),
                "cite targets were not delivered: {payload}"
            );
            let exposure_id = context["exposure_id"]
                .as_str()
                .expect("exposure_id")
                .to_string();

            for (id, outcome) in [
                (helped_id.as_str(), "helped"),
                (noop_id.as_str(), "noop"),
                (harmed_id.as_str(), "harmed"),
            ] {
                let cited = mcp_call(
                    &root,
                    "kimetsu_brain_cite",
                    json!({
                        "memory_id": id,
                        "exposure_id": exposure_id,
                        "outcome": outcome
                    }),
                );
                assert_eq!(cited["ok"], true, "{outcome}: {cited}");
                assert_eq!(cited["outcome"], outcome);
                assert_eq!(cited["grants"]["write"], json!(["repo"]));
            }

            let omitted = mcp_call(
                &root,
                "kimetsu_brain_cite",
                json!({
                    "memory_id": helped_id,
                    "exposure_id": exposure_id
                }),
            );
            assert_eq!(omitted["ok"], false, "a second outcome must conflict");

            let (_paths, _config, conn) = project::load_project(&root).unwrap();
            let after = |id: &str| -> f64 {
                conn.query_row(
                    "SELECT usefulness_score FROM memories WHERE memory_id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
            };
            assert!(
                after(&helped_id) > before_helped,
                "helped must raise usefulness"
            );
            assert_eq!(
                after(&noop_id),
                before_noop,
                "noop must not move usefulness"
            );
            assert!(
                after(&harmed_id) < before_harmed,
                "harmed must lower usefulness"
            );
            let reason: String = conn
                .query_row(
                    "SELECT invalidated_reason FROM memories WHERE memory_id=?1",
                    [harmed_id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(reason, "cite_harmed");
            drop(conn);

            plugin_uninstall(
                &root,
                BridgeTarget::parse("grok-bot").expect("grok-bot"),
                InstallScope::Workspace,
            )
            .expect("uninstall grok");
            assert!(!root.join(".kimetsu/hosts/grok-bot.md").exists());

            let _ = fs::remove_dir_all(root);
        });
    }

    #[test]
    fn grok_bot_remote_snippet_names_the_http_endpoint_and_no_config_file() {
        let root = temp_root("remote");
        plugin_install_remote(
            &root,
            BridgeTarget::parse("grok-bot").expect("grok-bot"),
            InstallScope::Workspace,
            PluginMode::Optional,
            &RemoteInstall {
                base_url: "https://brain.example".to_string(),
                repo_id: "repo-1".to_string(),
                token: None,
            },
        )
        .expect("remote grok");
        let grok = fs::read_to_string(root.join(".kimetsu/hosts/grok-bot.md")).expect("snippet");
        assert!(grok.contains("https://brain.example/mcp/repo-1"));
        assert!(grok.contains("${KIMETSU_REMOTE_TOKEN}"));
        assert!(grok.contains("not a Grok Bot config"));
        assert!(!grok.contains("\"mcpServers\""));
        assert!(grok.contains("helped") && grok.contains("harmed"));
        let _ = fs::remove_dir_all(root);
    }
}
