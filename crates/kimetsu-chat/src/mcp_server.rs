use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::bridge::{
    BridgeTarget, bridge_export_skill, bridge_import_skill, bridge_scan, bridge_sync,
    plugin_install,
};
use crate::skills::{SkillConfig, SkillRegistry, skill_origin_label};

#[derive(Debug, Clone)]
pub struct McpServeConfig {
    pub workspace: PathBuf,
    pub skills: SkillConfig,
}

impl McpServeConfig {
    pub fn new(workspace: PathBuf) -> Self {
        let skills = SkillConfig {
            include_user_roots: true,
            ..SkillConfig::default()
        };
        Self { workspace, skills }
    }
}

pub fn serve_mcp<R: BufRead, W: Write>(
    reader: R,
    mut writer: W,
    config: McpServeConfig,
) -> Result<(), String> {
    let workspace = config
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| config.workspace.clone());
    for line in reader.lines() {
        let line = line.map_err(|err| format!("read MCP stdin: {err}"))?;
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let request: Value =
            serde_json::from_str(line).map_err(|err| format!("parse MCP request: {err}"))?;
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let response = match handle_mcp_method(method, params, &workspace, &config.skills) {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": err,
                }
            }),
        };
        writeln!(writer, "{response}").map_err(|err| format!("write MCP stdout: {err}"))?;
        writer
            .flush()
            .map_err(|err| format!("flush MCP stdout: {err}"))?;
    }
    Ok(())
}

fn handle_mcp_method(
    method: &str,
    params: Value,
    workspace: &Path,
    skills: &SkillConfig,
) -> Result<Value, String> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "prompts": {},
            },
            "serverInfo": {
                "name": "kimetsu",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "tools/call missing name".to_string())?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let value = call_tool(name, arguments, workspace, skills)?;
            Ok(json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
                }]
            }))
        }
        "prompts/list" => Ok(json!({
            "prompts": [
                {
                    "name": "kimetsu_bridge_status",
                    "description": "Summarize Kimetsu bridge extension status for this workspace."
                },
                {
                    "name": "kimetsu_delegate",
                    "description": "Ask the host agent to delegate expensive coding loops to Kimetsu when useful."
                }
            ]
        })),
        "prompts/get" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let text = match name {
                "kimetsu_bridge_status" => {
                    "Use the kimetsu_bridge_status MCP tool and summarize installed, missing, portable, and blocked capabilities."
                }
                "kimetsu_delegate" => {
                    "When the task is a broad coding loop, consider calling Kimetsu MCP tools for bridge status, skills, memory, review, or delegation instead of expanding the host harness context."
                }
                _ => return Err(format!("unknown prompt `{name}`")),
            };
            Ok(json!({
                "description": name,
                "messages": [{
                    "role": "user",
                    "content": {
                        "type": "text",
                        "text": text
                    }
                }]
            }))
        }
        other => Err(format!("unsupported MCP method `{other}`")),
    }
}

fn call_tool(
    name: &str,
    arguments: Value,
    workspace: &Path,
    skills: &SkillConfig,
) -> Result<Value, String> {
    match name {
        "kimetsu_bridge_status" => {
            let scan = bridge_scan(workspace, skills)?;
            Ok(json!({
                "workspace": workspace,
                "skills": scan.skills.iter().map(|skill| json!({
                    "name": skill.name,
                    "description": skill.description,
                    "origin": skill.origin,
                    "kimetsu_extension": skill.kimetsu_extension,
                    "kimetsu_skill": skill.kimetsu_skill,
                    "claude_skill": skill.claude_skill,
                    "codex_skill": skill.codex_skill,
                })).collect::<Vec<_>>(),
                "extensions": scan.extensions.iter().map(|extension| json!({
                    "id": extension.manifest.id,
                    "name": extension.manifest.name,
                    "kind": extension.manifest.kind,
                    "source": extension.manifest.source,
                    "root": extension.root,
                })).collect::<Vec<_>>(),
            }))
        }
        "kimetsu_skills_search" => {
            let query = arguments.get("query").and_then(Value::as_str).unwrap_or("");
            let registry = SkillRegistry::discover(workspace, skills)?;
            Ok(json!({
                "query": query,
                "skills": registry.matching_skills(query).iter().map(|skill| json!({
                    "name": skill.name,
                    "description": skill.description,
                    "origin": skill_origin_label(skill),
                    "installed": registry.is_installed(skill),
                    "root": skill.root,
                    "entrypoint": skill.path,
                    "resources": skill.resource_summary(),
                })).collect::<Vec<_>>()
            }))
        }
        "kimetsu_bridge_import" => {
            let selection = string_arg(&arguments, "selection")?;
            let force = arguments
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let imported = bridge_import_skill(workspace, skills, &selection, force)?;
            Ok(json!({
                "imported": imported.manifest.name,
                "id": imported.manifest.id,
                "root": imported.root,
            }))
        }
        "kimetsu_bridge_export" => {
            let selection = string_arg(&arguments, "selection")?;
            let target = BridgeTarget::parse(&string_arg(&arguments, "target")?)?;
            let force = arguments
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let exported = bridge_export_skill(workspace, skills, &selection, target, force)?;
            Ok(json!({
                "exported": selection,
                "target": target.as_str(),
                "root": exported,
            }))
        }
        "kimetsu_bridge_sync" => {
            let force = arguments
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let imported = bridge_sync(workspace, skills, force)?;
            Ok(json!({ "imported": imported }))
        }
        "kimetsu_plugin_install" => {
            let target = BridgeTarget::parse(&string_arg(&arguments, "target")?)?;
            let force = arguments
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let report = plugin_install(workspace, target, force)?;
            Ok(json!({
                "target": report.target.as_str(),
                "files": report.files,
            }))
        }
        other => Err(format!("unknown Kimetsu MCP tool `{other}`")),
    }
}

fn string_arg(arguments: &Value, name: &str) -> Result<String, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing `{name}`"))
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "kimetsu_bridge_status",
            "description": "List Kimetsu bridge skills/extensions and where each is installed.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kimetsu_skills_search",
            "description": "Search skills across Kimetsu, Claude Code, Codex, Agents, and marketplace caches.",
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }
        },
        {
            "name": "kimetsu_bridge_import",
            "description": "Import a skill bundle into the canonical .kimetsu/extensions registry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selection": { "type": "string" },
                    "force": { "type": "boolean" }
                },
                "required": ["selection"]
            }
        },
        {
            "name": "kimetsu_bridge_export",
            "description": "Export a canonical or discovered skill to Claude Code, Codex, or Kimetsu skill roots.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "selection": { "type": "string" },
                    "target": { "type": "string", "enum": ["claude-code", "codex", "kimetsu"] },
                    "force": { "type": "boolean" }
                },
                "required": ["selection", "target"]
            }
        },
        {
            "name": "kimetsu_bridge_sync",
            "description": "Import all discovered non-Kimetsu skills into .kimetsu/extensions.",
            "inputSchema": {
                "type": "object",
                "properties": { "force": { "type": "boolean" } }
            }
        },
        {
            "name": "kimetsu_plugin_install",
            "description": "Install Kimetsu as a local MCP plugin for Claude Code or Codex in this workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["claude-code", "codex"] },
                    "force": { "type": "boolean" }
                },
                "required": ["target"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_tools() {
        let result = handle_mcp_method(
            "tools/list",
            json!({}),
            Path::new("."),
            &SkillConfig::default(),
        )
        .expect("tools/list");
        assert!(
            result["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["name"].as_str() == Some("kimetsu_bridge_status") })
        );
    }
}
