use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::{benchmark, project};
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use serde_json::{Value, json};

use crate::bridge::{
    BridgeTarget, InstallScope, PluginMode, bridge_export_skill, bridge_import_skill, bridge_scan,
    bridge_sync, plugin_install,
};
use crate::skills::{SkillConfig, SkillRegistry, skill_origin_label};

const KIMETSU_MCP_INSTRUCTIONS: &str = "Kimetsu is a persistent brain sidecar for Claude Code and Codex. It accumulates generalizable knowledge across sessions and retrieves it on demand. Recommended workflow: (1) Call kimetsu_brain_context early on non-trivial tasks — if skipped:true is returned, the brain has nothing relevant and you paid zero overhead. (2) After solving a non-obvious problem that took real effort, call kimetsu_brain_record with a concrete lesson and 2-5 domain tags. Do NOT call for trivial or well-known knowledge. (3) For Terminal-Bench tasks, call kimetsu_benchmark_context instead — it prioritizes semantic_operator and anti_pattern memories over episodic summaries. Use kimetsu_bridge_status and kimetsu_skills_search when portable skills may help. Brain tools retrieve and curate durable context; bridge tools discover capabilities.";

const BRAIN_STATUS_DESCRIPTION: &str = "Inspect the Kimetsu brain for this workspace. Use this to see whether brain.db is initialized, how many memories/runs/proposals exist, and which memories have positive outcome usefulness. Call before relying on memory if you need to know whether the brain has signal.";

const BRAIN_CONTEXT_DESCRIPTION: &str = "Primary Kimetsu brain tool. Call early on non-trivial tasks with a concise task query to retrieve broker-ranked context capsules: accepted memories, repo snippets, manifests, and usefulness-weighted signals. Returns skipped:true (zero tokens) when no capsule is relevant above min_score threshold — safe to call on every non-trivial task without overhead concern.";

// TODO (v0.6+): fold benchmark-tag filtering + semantic_operator/anti_pattern
// preference into kimetsu_brain_context so kimetsu_benchmark_context becomes
// redundant. The split today exists because gauntlets accumulate episodic
// "run X scored Y" memories fast and pollute generic retrieval. Once the
// broker can apply the bench filter on-demand (e.g. a `prefer_roles=...` arg,
// or implicit detection of bench task slugs), benchmark_context can be
// removed and downstream tools (kimetsu-bench/) stop having to nudge Claude
// toward bench-specific MCP surface.
const BENCHMARK_CONTEXT_DESCRIPTION: &str = "Benchmark-specific Kimetsu brain tool. Use first for Terminal-Bench tasks. It detects or accepts a task slug, retrieves broker-ranked context with benchmark tags, prioritizes accepted semantic_operator and anti_pattern memories over exact episodic run summaries, and returns a compact playbook that Codex/Claude Code should follow before broad exploration. Set warm_policy to cold_brain, reactive_warm, or full_warm to match the benchmark condition being measured.";

const BENCHMARK_RECORD_OUTCOME_DESCRIPTION: &str = "Record a benchmark attempt in Kimetsu brain. This always writes an accepted memory_role=episodic outcome summary for the exact task. Optionally pass generalized_memory with memory_role=semantic_operator or anti_pattern to create a pending human-review memory proposal for a reusable tactic or warning that should transfer beyond one task slug.";

const BRAIN_MEMORY_LIST_DESCRIPTION: &str = "List recent accepted Kimetsu memories with confidence, use count, and usefulness score. Use when you need to understand the durable memory pool or pick a memory id for invalidation.";

const BRAIN_MEMORY_TOP_DESCRIPTION: &str = "List outcome-ranked Kimetsu memories by usefulness_score/use_count. Use this to see which memories have actually helped previous runs and should be trusted more than fresh or low-signal memories.";

const BRAIN_MEMORY_ADD_DESCRIPTION: &str = "Add a durable Kimetsu memory manually. Use only when the user states a reusable preference, convention, command, failure pattern, or fact that should influence future runs. This writes a memory.accepted event.";

const BRAIN_MEMORY_PROPOSALS_DESCRIPTION: &str = "List memory proposals waiting for curation. Use after Kimetsu or a benchmark generated proposed memories so Codex/Claude Code can help review what should become durable brain state.";

const BRAIN_MEMORY_ACCEPT_DESCRIPTION: &str = "Accept a pending Kimetsu memory proposal and promote it into durable memory. Use only when the proposal is reusable beyond the immediate task. This writes a memory.accepted event.";

const BRAIN_MEMORY_REJECT_DESCRIPTION: &str = "Reject a pending Kimetsu memory proposal. Use when the proposal is too task-specific, wrong, duplicated, or unsafe to reuse. This writes a memory.rejected event.";

const BRAIN_MEMORY_INVALIDATE_DESCRIPTION: &str = "Retire an accepted Kimetsu memory so the broker stops retrieving it. Use when a memory is stale, wrong, harmful, or contradicted by newer evidence. This writes a memory.invalidated event.";

const BRAIN_MEMORY_BLAME_DESCRIPTION: &str = "Per-run memory attribution. Pass a run_id (ULID printed in chat sessions / trace files) to surface which memories the model explicitly cited via the cite_memory tool (strong ±1.0 usefulness signal) vs which were silently retrieved but never cited (weak ±0.1 signal). Use after a run feels off to learn whether a misleading memory was responsible, or after a clean run to see which memories actually earned their keep.";

const BRAIN_MEMORY_CONFLICTS_DESCRIPTION: &str = "List open memory-conflict hits detected at ingest. When a new memory's embedding is close (cosine >= 0.8 by default) to an existing memory in the same scope but their normalized text differs, the brain logs a conflict so an operator can decide which version to keep. Returns up to `limit` conflicts (default 50) merged from project + user brains. Use this before adding contradictory guidance so you don't end up with both 'use anyhow' and 'use thiserror' silently competing in retrieval.";

const BRAIN_INGEST_REPO_DESCRIPTION: &str = "Index the repository into Kimetsu brain.db so future kimetsu_brain_context calls can retrieve repo snippets and manifests. Use during setup or after major repo changes. This writes repo.ingested events.";

const BRIDGE_STATUS_DESCRIPTION: &str = "Use when deciding whether portable skills/extensions can help the current task. Lists capabilities discovered in this workspace and across user harness roots, plus where each is installed for Kimetsu, Codex, and Claude Code. For Terminal-Bench memory/context, prefer kimetsu_benchmark_context first; for other work, prefer kimetsu_brain_context first.";

const SKILLS_SEARCH_DESCRIPTION: &str = "Search Kimetsu's cross-harness skill catalog for task-specific instructions. Use concise task keywords such as 'terminal-bench mips interpreter', 'github review', or 'phaser game'. Results include root and SKILL.md entrypoint paths; after selecting a relevant result, read its entrypoint with the host harness file tools and follow it.";

const BRIDGE_IMPORT_DESCRIPTION: &str = "Import a discovered skill into this workspace's canonical .kimetsu/extensions registry so Kimetsu can track and re-export it. Use after kimetsu_skills_search or kimetsu_bridge_status identifies a useful external skill that should become portable. This writes files; keep force=false unless replacing an existing import intentionally.";

const BRIDGE_EXPORT_DESCRIPTION: &str = "Export a canonical or discovered skill into a target harness skill root: codex, claude-code, or kimetsu. Use when the current harness needs a skill that Kimetsu found elsewhere, or before running a benchmark where that harness should see the skill natively. This writes files; keep force=false unless replacing intentionally.";

const BRIDGE_SYNC_DESCRIPTION: &str = "Bulk-import all discovered non-Kimetsu skills into .kimetsu/extensions. Use for setup or migration, not during a narrow task unless the user asked to synchronize capabilities. This writes files and may touch many skill bundles.";

const PLUGIN_INSTALL_DESCRIPTION: &str = "Install Kimetsu MCP/plugin wiring for a target harness in this workspace. For codex, writes .codex/config.toml, .codex/hooks.json, the kimetsu-bridge skill, and the kimetsu-memory-harvester custom agent; for claude-code, writes .mcp.json, command docs, and .claude/settings.json hooks. Set mode=optional to recommend brain-first usage, or mode=required to tell the host harness that non-trivial work must load Kimetsu brain context. Installed guidance tells benchmark agents to prefer kimetsu_benchmark_context and record outcomes through kimetsu_benchmark_record_outcome. Set scope=workspace (default) to install into this workspace, or scope=global to install into the user's home (~/.claude, ~/.claude.json, ~/.codex) for all sessions. Existing user hooks are preserved (merged, not replaced).";

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
    // v0.8: honor the [embedder] config (env still wins) before any
    // retrieval initializes the process-static embedder. A server
    // started after `model set` therefore loads the configured model.
    if let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&workspace)
        && let Ok(project_config) = project::load_config(&paths)
    {
        kimetsu_brain::embeddings::apply_embedder_selection(Some(&project_config.embedder.model));
    }
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

/// Transport-agnostic MCP method dispatch.
///
/// `allowed_tools = None` → full catalog (identical to the previous
/// `handle_mcp_method` behaviour; stdio path uses this).
///
/// `allowed_tools = Some(set)`:
///   - `"tools/list"` returns only entries whose `name` ∈ set.
///   - `"tools/call"` returns an error before dispatching if the
///     requested tool name is not in the set.
pub fn dispatch(
    method: &str,
    params: serde_json::Value,
    workspace: &std::path::Path,
    skills: &SkillConfig,
    allowed_tools: Option<&std::collections::BTreeSet<&'static str>>,
) -> Result<serde_json::Value, String> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "prompts": {},
            },
            "instructions": KIMETSU_MCP_INSTRUCTIONS,
            "serverInfo": {
                "name": "kimetsu",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "tools/list" => {
            let all = tool_definitions();
            let tools = match allowed_tools {
                None => all,
                Some(set) => {
                    let filtered: Vec<Value> = all
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|entry| {
                            entry
                                .get("name")
                                .and_then(Value::as_str)
                                .map(|n| set.contains(n))
                                .unwrap_or(false)
                        })
                        .collect();
                    Value::Array(filtered)
                }
            };
            Ok(json!({ "tools": tools }))
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "tools/call missing name".to_string())?;
            if let Some(set) = allowed_tools {
                if !set.contains(name) {
                    return Err(format!("tool `{name}` is not available in remote mode"));
                }
            }
            // `allowed_tools.is_some()` is the remote-mode marker (see the
            // dispatch docstring): remote serves cloned repos whose
            // project.toml is untrusted, so only the operator-set env var
            // can enable writes there. Local stdio consults project config
            // (default: enabled — recording lessons is the product's own
            // prescribed workflow).
            if is_privileged_write_tool(name)
                && !mcp_write_tools_enabled(workspace, allowed_tools.is_some())
            {
                return Err(format!(
                    "tool `{name}` requires explicit approval; enable with \
                     `kimetsu config set kimetsu.mcp_write_tools true` (local) or \
                     KIMETSU_MCP_ENABLE_WRITE_TOOLS=1 (env override, required for remote)"
                ));
            }
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
                    "name": "kimetsu_brain_context",
                    "description": "Recommended first step for broad tasks: retrieve Kimetsu brain capsules for the task."
                },
                {
                    "name": "kimetsu_benchmark_context",
                    "description": "Recommended first step for Terminal-Bench tasks: retrieve a compact Kimetsu benchmark playbook."
                },
                {
                    "name": "kimetsu_bridge_status",
                    "description": "Explain what portable Kimetsu skills/extensions are visible to this harness."
                },
                {
                    "name": "kimetsu_delegate",
                    "description": "Explain how to use Kimetsu brain plus bridge tools as a sidecar before broad coding, review, or benchmark work."
                }
            ]
        })),
        "prompts/get" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let text = match name {
                "kimetsu_brain_context" => {
                    "Call kimetsu_brain_context early on non-trivial coding, review, setup, or benchmark tasks. Use a concise query containing the task goal and relevant technologies. Treat returned memory/repo/manifest capsules as working context, then continue with normal host harness file, shell, edit, and verification tools."
                }
                "kimetsu_benchmark_context" => {
                    "For Terminal-Bench tasks, call kimetsu_benchmark_context with the task text and dataset before broad exploration. Use the returned playbook_markdown as working memory. After the attempt, call kimetsu_benchmark_record_outcome with status, commands, pitfalls, and verification so future runs retrieve the learned path."
                }
                "kimetsu_bridge_status" => {
                    "Call kimetsu_bridge_status when the task might need a portable skill or extension. Summarize which skills/extensions are visible, which harness roots already have them, and what the next Kimetsu bridge action should be, if any. For Terminal-Bench memory/context, call kimetsu_benchmark_context; for other work, call kimetsu_brain_context."
                }
                "kimetsu_delegate" => {
                    "Use Kimetsu as a sidecar rather than a replacement for normal tools. First call kimetsu_benchmark_context for Terminal-Bench tasks or kimetsu_brain_context for other retrieved memory/repo context. Then call kimetsu_bridge_status and kimetsu_skills_search if the task could benefit from a reusable skill. Import/export/sync only for setup or when the host harness needs a missing skill installed."
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

/// Thin wrapper for the stdio path: full catalog, no allowlist.
fn handle_mcp_method(
    method: &str,
    params: Value,
    workspace: &Path,
    skills: &SkillConfig,
) -> Result<Value, String> {
    dispatch(method, params, workspace, skills, None)
}

fn call_tool(
    name: &str,
    arguments: Value,
    workspace: &Path,
    skills: &SkillConfig,
) -> Result<Value, String> {
    match name {
        "kimetsu_brain_status" => Ok(kimetsu_brain_status(workspace)),
        "kimetsu_brain_insights" => Ok(kimetsu_brain_insights(workspace, &arguments)),
        "kimetsu_brain_context" => Ok(kimetsu_brain_context(workspace, &arguments)),
        "kimetsu_brain_record" => Ok(kimetsu_brain_record(workspace, &arguments)),
        "kimetsu_benchmark_context" => Ok(kimetsu_benchmark_context(workspace, &arguments)),
        "kimetsu_benchmark_record_outcome" => {
            kimetsu_benchmark_record_outcome(workspace, &arguments)
        }
        "kimetsu_brain_memory_list" => kimetsu_brain_memory_list(workspace, &arguments),
        "kimetsu_brain_memory_top" => kimetsu_brain_memory_top(workspace, &arguments),
        "kimetsu_brain_memory_add" => kimetsu_brain_memory_add(workspace, &arguments),
        "kimetsu_brain_memory_proposals" => kimetsu_brain_memory_proposals(workspace, &arguments),
        "kimetsu_brain_memory_accept" => kimetsu_brain_memory_accept(workspace, &arguments),
        "kimetsu_brain_memory_reject" => kimetsu_brain_memory_reject(workspace, &arguments),
        "kimetsu_brain_memory_invalidate" => kimetsu_brain_memory_invalidate(workspace, &arguments),
        "kimetsu_brain_memory_blame" => kimetsu_brain_memory_blame(workspace, &arguments),
        "kimetsu_brain_memory_conflicts" => kimetsu_brain_memory_conflicts(workspace, &arguments),
        "kimetsu_brain_ingest_repo" => kimetsu_brain_ingest_repo(workspace, &arguments),
        "kimetsu_bridge_status" => {
            let scan = bridge_scan(workspace, skills)?;
            Ok(json!({
                "usage": {
                    "summary": "Kimetsu bridge is available for cross-harness skills and extensions. For Terminal-Bench memory and retrieved context, call kimetsu_benchmark_context; for other work, call kimetsu_brain_context.",
                    "recommended_workflow": [
                        "Use kimetsu_benchmark_context first for Terminal-Bench tasks so Kimetsu can return a task-aware playbook.",
                        "Use kimetsu_brain_context first for other tasks that need remembered context or repo context.",
                        "Use kimetsu_skills_search with task keywords when you need task-specific guidance.",
                        "Use kimetsu_bridge_export only when a discovered skill must be installed into codex, claude-code, or kimetsu.",
                        "Continue the task with the host harness's normal file, shell, and edit tools after loading the relevant skill entrypoint."
                    ]
                },
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
                "usage": {
                    "recommended_next_steps": [
                        "Pick the most relevant skill by name, description, and origin.",
                        "Read the returned entrypoint path, usually SKILL.md, with the host harness file tools.",
                        "Follow that skill's workflow; import or export it only if the harness needs it installed persistently."
                    ]
                },
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
            let scope = arguments
                .get("scope")
                .and_then(Value::as_str)
                .map(InstallScope::parse)
                .transpose()?
                .unwrap_or_default();
            if matches!(scope, InstallScope::Global) {
                return Err("global plugin install is not available through MCP; run the explicit CLI command instead".to_string());
            }
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
        "kimetsu_brain_model_list" => kimetsu_brain_model_list(workspace),
        "kimetsu_brain_model_set" => kimetsu_brain_model_set(workspace, &arguments),
        "kimetsu_brain_reindex" => kimetsu_brain_reindex(workspace, &arguments),
        "kimetsu_brain_memory_search" => kimetsu_brain_memory_search(workspace, &arguments),
        "kimetsu_brain_conflict_resolve" => kimetsu_brain_conflict_resolve(workspace, &arguments),
        "kimetsu_brain_prune" => kimetsu_brain_prune(workspace, &arguments),
        "kimetsu_brain_config_show" => kimetsu_brain_config_show(workspace),
        other => Err(format!("unknown Kimetsu MCP tool `{other}`")),
    }
}

fn is_privileged_write_tool(name: &str) -> bool {
    matches!(
        name,
        "kimetsu_brain_record"
            | "kimetsu_benchmark_record_outcome"
            | "kimetsu_brain_memory_add"
            | "kimetsu_brain_memory_accept"
            | "kimetsu_brain_memory_reject"
            | "kimetsu_brain_memory_invalidate"
            | "kimetsu_brain_ingest_repo"
            | "kimetsu_bridge_import"
            | "kimetsu_bridge_export"
            | "kimetsu_bridge_sync"
            | "kimetsu_plugin_install"
            | "kimetsu_brain_model_set"
            | "kimetsu_brain_reindex"
            | "kimetsu_brain_conflict_resolve"
            | "kimetsu_brain_prune"
    )
}

fn mcp_write_tools_enabled(workspace: &Path, remote: bool) -> bool {
    let env = std::env::var("KIMETSU_MCP_ENABLE_WRITE_TOOLS").ok();
    let config_allow = kimetsu_core::paths::ProjectPaths::discover(workspace)
        .ok()
        .and_then(|paths| project::load_config(&paths).ok())
        .map(|config| config.kimetsu.mcp_write_tools);
    write_tools_decision(env.as_deref(), remote, config_allow)
}

/// v1.0.0: pure decision for the privileged-write gate.
///
/// Precedence:
///   1. The env var, when SET, always wins — truthy enables, anything else
///      disables. This is the operator override in both directions.
///   2. Remote mode (env unset): always deny. The workspace config on a
///      remote server comes from a cloned repo — untrusted input must not
///      be able to enable writes.
///   3. Local mode (env unset): the project's `kimetsu.mcp_write_tools`
///      (default true — a local plugin install IS the trusted session, and
///      the brain's own workflow instructs the agent to record lessons).
///      An unreadable config also defaults to true.
fn write_tools_decision(env: Option<&str>, remote: bool, config_allow: Option<bool>) -> bool {
    if let Some(value) = env {
        return matches!(
            value.trim(),
            "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
        );
    }
    if remote {
        return false;
    }
    config_allow.unwrap_or(true)
}

fn kimetsu_brain_status(workspace: &Path) -> Value {
    let Ok((paths, config, conn)) = project::load_project(workspace) else {
        return brain_unavailable_json(
            workspace,
            "Kimetsu brain is not initialized for this workspace or project.toml/brain.db could not be opened.",
        );
    };
    let repo_root = paths
        .repo_root
        .canonicalize()
        .unwrap_or_else(|_| paths.repo_root.clone())
        .to_string_lossy()
        .to_string();
    let indexed_files: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_files WHERE repo_root = ?1",
            [&repo_root],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let indexed_manifests: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM repo_manifests WHERE repo_root = ?1",
            [&repo_root],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let memories = project::list_memories(workspace).unwrap_or_default();
    let runs = project::list_runs(workspace).unwrap_or_default();
    let pending = project::list_proposals(
        workspace,
        project::ProposalFilter {
            status: Some("pending".to_string()),
            limit: 20,
            ..project::ProposalFilter::default()
        },
    )
    .unwrap_or_default();
    let top_memories = project::list_memories_top(
        workspace,
        project::TopOptions {
            min_uses: 1,
            limit: 10,
            ..project::TopOptions::default()
        },
    )
    .unwrap_or_default();

    json!({
        "initialized": true,
        "project_id": config.kimetsu.project_id,
        "repo_root": paths.repo_root,
        "brain_db": paths.brain_db,
        "counts": {
            "memories": memories.len(),
            "runs": runs.len(),
            "pending_proposals": pending.len(),
            "repo_indexed_files_for_current_root": indexed_files,
            "repo_indexed_manifests_for_current_root": indexed_manifests,
        },
        "usage": {
            "primary_next_step": "For Terminal-Bench tasks, call kimetsu_benchmark_context with the task text and dataset. For other tasks, call kimetsu_brain_context with the current task query to retrieve broker-ranked memory and repo capsules.",
            "repo_indexing": "If repo_indexed_files_for_current_root is 0, call kimetsu_brain_ingest_repo before expecting repo_file capsules. This matters across Windows, WSL, and container path roots.",
            "curation": "Use kimetsu_brain_memory_proposals plus accept/reject tools to manage new memories; use kimetsu_brain_memory_invalidate for stale or harmful memories."
        },
        "top_memories": top_memories.iter().map(json_memory_row).collect::<Vec<_>>(),
        "pending_proposals": pending.iter().map(json_proposal_row).collect::<Vec<_>>(),
    })
}

/// v1.0 (C6): `kimetsu_brain_insights` — effectiveness analytics MCP tool.
fn kimetsu_brain_insights(workspace: &Path, arguments: &Value) -> Value {
    use kimetsu_brain::analytics::{self, InsightsOptions};

    let last_n_runs = u32_arg(arguments, "last_n_runs", 50, 1, u32::MAX);
    let since = optional_string_arg(arguments, "since");
    let top = u32_arg(arguments, "top", 10, 1, u32::MAX);

    let opts = InsightsOptions {
        last_n_runs,
        since,
        top_n: top,
    };

    let report = match analytics::compute_insights(workspace, opts) {
        Ok(r) => r,
        Err(err) => {
            return brain_unavailable_json(workspace, &format!("kimetsu_brain_insights: {err}"));
        }
    };

    // Build a short interpretation string from headline numbers.
    let citation_rate = report
        .citation
        .citation_rate
        .map(|v| format!("{:.1}%", v * 100.0))
        .unwrap_or_else(|| "n/a".to_string());
    let acceptance_rate = report
        .proposals
        .acceptance_rate
        .map(|v| format!("{:.1}%", v * 100.0))
        .unwrap_or_else(|| "n/a".to_string());
    let avg_tokens = report
        .token_economy
        .avg_injected_tokens
        .map(|v| format!("{:.0} tokens/injection", v))
        .unwrap_or_else(|| "n/a tokens/injection".to_string());
    let interpretation = format!(
        "Citation rate {citation_rate} ({cited}/{retrieved} memories cited), \
         proposal acceptance {acceptance_rate} ({accepted}/{total} decided), \
         token economy {avg_tokens}. Retrieval hit-rate n/a until C7.",
        cited = report.citation.cited_total,
        retrieved = report.citation.retrieved_total,
        accepted = report.proposals.accepted,
        total = report.proposals.accepted + report.proposals.rejected,
    );

    let report_json = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    json!({
        "ok": true,
        "report": report_json,
        "interpretation": interpretation,
    })
}

/// v0.7: shared argument parsing for retrieval MCP tools. Callers pass
/// their own defaults for `budget_tokens` and `max_capsules` since bench
/// and brain use different values (2500/8 vs 6000/3).
struct SharedRetrievalArgs {
    stage: String,
    budget_tokens: u32,
    max_capsules: usize,
}

fn parse_shared_retrieval_args(
    arguments: &Value,
    default_budget: u32,
    default_max_capsules: u32,
) -> SharedRetrievalArgs {
    SharedRetrievalArgs {
        stage: arguments
            .get("stage")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .unwrap_or("localization")
            .to_string(),
        budget_tokens: u32_arg(arguments, "budget_tokens", default_budget, 500, 30000),
        max_capsules: u32_arg(arguments, "max_capsules", default_max_capsules, 1, 20) as usize,
    }
}

fn kimetsu_brain_context(workspace: &Path, arguments: &Value) -> Value {
    match brain_context_tool(workspace, arguments, None) {
        Ok(v) => v,
        Err(e) => brain_unavailable_json(workspace, &e),
    }
}

/// Candidate pool the remote reranker judges before truncating to the caller's
/// cap. Mirrors `RERANK_POOL` in `kimetsu-cli/src/embed_daemon/server.rs`.
pub const REMOTE_RERANK_POOL: usize = 6;

/// Sigmoid-score floor for the remote reranker — capsules scored below this
/// are noise. Mirrors `RERANK_FLOOR` in `kimetsu-cli/src/embed_daemon/server.rs`.
pub const REMOTE_RERANK_FLOOR: f32 = 0.30;

/// Transport-agnostic body of the `kimetsu_brain_context` tool.
///
/// When `reranker` is `None` the behaviour is identical to the previous
/// private implementation (used by the stdio MCP path). When `Some`:
/// - over-fetches a larger candidate pool (`max_capsules = cap.max(REMOTE_RERANK_POOL)`)
/// - bumps `budget_tokens` to at least 6000 so the pool isn't token-starved
/// - retrieves, then calls `rerank_capsules` before serialising
///
/// The JSON response shape is byte-compatible with the `None` path so
/// existing tests and the stdio consumer are unaffected.
pub fn brain_context_tool(
    workspace: &Path,
    arguments: &serde_json::Value,
    reranker: Option<&dyn kimetsu_brain::embeddings::Reranker>,
) -> Result<serde_json::Value, String> {
    use kimetsu_brain::context::{rerank_capsules, ContextRequest};

    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        return Ok(json!({
            "ok": false,
            "error": "missing `query`",
            "usage": "Pass a concise task description, e.g. {\"query\":\"terminal-bench mips interpreter create frame.bmp\",\"stage\":\"implementation\"}."
        }));
    }
    let shared = parse_shared_retrieval_args(arguments, 6000, 3);
    let stage = shared.stage.as_str();
    let budget_tokens = shared.budget_tokens;
    let cap = shared.max_capsules;
    // v0.6: score threshold and role preference controls.
    let min_score = arguments
        .get("min_score")
        .and_then(Value::as_f64)
        .map(|v| v.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.15);
    let tags: Vec<String> = arguments
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let prefer_roles: Vec<String> = arguments
        .get("prefer_roles")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // v0.4.4: auto-collect ambient workspace context (git branch,
    // dirty files, recent edits) and append a short retrieval
    // suffix to the query. Callers can disable per-call by passing
    // `include_ambient: false`, OR globally via
    // `KIMETSU_BRAIN_AMBIENT=off`. The full ambient block is
    // surfaced in the response so the model knows what augmented
    // its retrieval.
    // W3.2: load broker.ambient from the project config (best-effort;
    // default true keeps existing behavior when config is missing).
    let config_ambient = load_config_ambient(workspace);
    let (effective_query, ambient_payload) = augment_with_ambient(
        workspace,
        query,
        arguments,
        "include_ambient",
        config_ambient,
    );

    // When reranking, over-fetch a larger candidate pool so the cross-encoder
    // sees enough diversity before truncating to `cap`, and bump the token
    // budget so the pool isn't starved. Same logic as the embed daemon.
    let (fetch_cap, fetch_budget) = if reranker.is_some() {
        (cap.max(REMOTE_RERANK_POOL), budget_tokens.max(6000))
    } else {
        (cap, budget_tokens)
    };

    let request = ContextRequest {
        stage: stage.to_string(),
        query: effective_query.clone(),
        budget_tokens: fetch_budget,
        tags,
        min_score,
        max_capsules: fetch_cap,
        prefer_roles,
        ..Default::default()
    };

    match project::retrieve_context_readonly_with_request(workspace, request) {
        Ok(bundle) if bundle.skipped => Ok(json!({
            "ok": true,
            "skipped": true,
            "top_score": bundle.top_score,
            "min_score": min_score,
            "capsule_count": 0,
            "capsules": [],
            "usage": {
                "how_to_use": "Brain has no capsules above the relevance threshold for this query. Proceed without brain context — this call cost nothing."
            }
        })),
        Ok(mut bundle) => {
            // Apply cross-encoder reranking when a reranker is present.
            if let Some(rr) = reranker {
                bundle.capsules =
                    rerank_capsules(&effective_query, bundle.capsules, rr, REMOTE_RERANK_FLOOR, cap);
            }
            Ok(json!({
                "ok": true,
                "skipped": false,
                "top_score": bundle.top_score,
                "usage": {
                    "how_to_use": "Read capsule summaries before planning. Memory capsules are durable Kimetsu brain state; repo_file and repo_manifest capsules point to likely relevant files/manifests.",
                    "next_steps": [
                        "Use returned expansion_handle values as provenance when deciding what files or memories matter.",
                        "If capsule_count is 0 or repo capsules are missing, call kimetsu_brain_status and then kimetsu_brain_ingest_repo if repo_indexed_files_for_current_root is 0.",
                        "Continue with the host harness's normal file/shell/edit tools.",
                        "If a memory is stale or harmful, call kimetsu_brain_memory_invalidate with its memory id."
                    ]
                },
                "stage": bundle.stage,
                "query": query,
                "augmented_query": effective_query,
                "ambient": ambient_payload,
                "budget_tokens": bundle.budget_tokens,
                "used_tokens": bundle.used_tokens,
                "capsule_count": bundle.capsules.len(),
                "excluded_count": bundle.excluded.len(),
                "capsules": bundle.capsules,
                "excluded": bundle.excluded,
            }))
        }
        Err(err) => Ok(brain_unavailable_json(workspace, &err.to_string())),
    }
}

/// v0.6: general-purpose capture tool. Records a concrete, reusable
/// lesson into the brain — the counterpart to `kimetsu_brain_context`.
/// Use after solving a non-obvious problem that required real effort or
/// that you'd want to remember next session. Do NOT call for trivial or
/// well-known knowledge already in training data.
fn kimetsu_brain_record(workspace: &Path, arguments: &Value) -> Value {
    let lesson = match arguments.get("lesson").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => {
            return json!({
                "ok": false,
                "error": "missing `lesson`",
                "usage": "Pass a concrete, actionable rule, e.g. {\"lesson\":\"Never open a SQLite WAL DB before fixing the WAL — it deletes the WAL on close\",\"tags\":[\"sqlite\",\"wal\",\"recovery\"]}."
            });
        }
    };
    let tags: Vec<String> = arguments
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let context_note = arguments
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let confidence = arguments
        .get("confidence")
        .and_then(Value::as_f64)
        .map(|v| v.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.8);
    let kind_str = arguments
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("semantic_operator");

    // Prefix tags into the lesson text so FTS picks them up without schema change.
    let tag_prefix = if tags.is_empty() {
        String::new()
    } else {
        format!("[tags: {}] ", tags.join(" "))
    };
    let full_text = if context_note.is_empty() {
        format!("{tag_prefix}{lesson}")
    } else {
        format!("{tag_prefix}{lesson} (context: {context_note})")
    };

    let kind = match kind_str {
        "anti_pattern" => MemoryKind::FailurePattern,
        "convention" => MemoryKind::Convention,
        _ => MemoryKind::Fact, // semantic_operator and default both stored as Fact
    };

    match project::propose_or_merge_memory(
        workspace,
        MemoryScope::Project,
        kind,
        &full_text,
        confidence,
        "lesson from kimetsu_brain_record",
    ) {
        Ok(project::ProposeResult::Added(memory_id)) => json!({
            "ok": true,
            "memory_id": memory_id,
            "result": "added",
            "kind": kind_str,
            "lesson": lesson,
            "tags": tags,
            "usage": "Memory accepted into brain. Future kimetsu_brain_context calls with matching queries will retrieve it."
        }),
        Ok(project::ProposeResult::Merged(memory_id)) => json!({
            "ok": true,
            "memory_id": memory_id,
            "result": "merged",
            "kind": kind_str,
            "lesson": lesson,
            "tags": tags,
            "usage": "Similar memory already exists and was updated with this lesson. No duplicate created."
        }),
        Ok(project::ProposeResult::Duplicate(memory_id)) => json!({
            "ok": true,
            "memory_id": memory_id,
            "result": "duplicate",
            "kind": kind_str,
            "lesson": lesson,
            "tags": tags,
            "usage": "Identical memory already in brain. No write needed."
        }),
        Ok(project::ProposeResult::Proposed(proposal_id)) => json!({
            "ok": true,
            "proposal_id": proposal_id,
            "result": "proposed",
            "kind": kind_str,
            "lesson": lesson,
            "tags": tags,
            "usage": "Memory proposed for review (low confidence). Run `kimetsu brain memory review` to accept or reject."
        }),
        Err(err) => json!({ "ok": false, "error": err.to_string() }),
    }
}

/// W3.2: load `broker.ambient` from the project config, best-effort.
/// Returns `true` (the default) if the config is missing or unreadable
/// so existing behavior is preserved when the project hasn't been
/// initialized or the toml is absent.
fn load_config_ambient(workspace: &Path) -> bool {
    kimetsu_core::paths::ProjectPaths::discover(workspace)
        .ok()
        .and_then(|paths| project::load_config(&paths).ok())
        .map(|cfg| cfg.broker.ambient)
        .unwrap_or(true)
}

/// v0.4.4: shared ambient-augmentation helper for the brain + benchmark
/// MCP tools.
///
/// Returns `(effective_query, ambient_payload)`. The payload is JSON
/// (or `null` when ambient is disabled either per-call or globally),
/// safe to embed directly into the response.
///
/// W3.2: `config_ambient` is the project config's `broker.ambient` value
/// (default true). Resolution: `KIMETSU_BRAIN_AMBIENT` env > `config_ambient`.
fn augment_with_ambient(
    workspace: &Path,
    query: &str,
    arguments: &Value,
    arg_key: &str,
    config_ambient: bool,
) -> (String, Value) {
    let include = bool_arg(arguments, arg_key, true);
    if !include || !kimetsu_brain::ambient::ambient_enabled_with(config_ambient) {
        return (query.to_string(), json!(null));
    }
    let ctx = kimetsu_brain::ambient::collect(workspace);
    let augmented = kimetsu_brain::ambient::augment_query(query, &ctx);
    let payload = serde_json::to_value(&ctx).unwrap_or(json!(null));
    (augmented, payload)
}

/// Prefer `kimetsu_brain_context` with `prefer_roles: ["semantic_operator","anti_pattern"]`
/// in new code; this wrapper will be removed in v0.8.
fn kimetsu_benchmark_context(workspace: &Path, arguments: &Value) -> Value {
    let task = arguments
        .get("task")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("query").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if task.is_empty() {
        return json!({
            "ok": false,
            "error": "missing `task`",
            "usage": "Pass the Terminal-Bench instruction text, e.g. {\"task\":\"compile-compcert build task\",\"dataset\":\"terminal-bench/terminal-bench-2\"}."
        });
    }

    let dataset = optional_string_arg(arguments, "dataset")
        .unwrap_or_else(|| benchmark::DEFAULT_BENCHMARK_DATASET.to_string());
    let task_slug = optional_string_arg(arguments, "task_slug")
        .or_else(|| optional_string_arg(arguments, "slug"));
    let warm_policy = optional_string_arg(arguments, "warm_policy")
        .or_else(|| optional_string_arg(arguments, "brain_warm_policy"))
        .as_deref()
        .and_then(benchmark::BenchmarkWarmPolicy::parse)
        .unwrap_or_default();
    let require_benchmark_memory = bool_arg(arguments, "require_benchmark_memory", false);

    let shared = parse_shared_retrieval_args(arguments, 2500, 8);
    let stage = shared.stage.as_str();
    let budget_tokens = shared.budget_tokens;
    let max_capsules = shared.max_capsules;

    // v0.4.4: collect ambient context and pass its rendered suffix
    // into the brain so it appends AFTER slug detection (otherwise
    // the suffix would confuse `normalize_task_slug`). The full
    // ambient block is also surfaced in the response payload.
    // W3.2: honor broker.ambient from project config with env override.
    let config_ambient = load_config_ambient(workspace);
    let include_ambient = bool_arg(arguments, "include_ambient", true);
    let ambient_ctx =
        if include_ambient && kimetsu_brain::ambient::ambient_enabled_with(config_ambient) {
            Some(kimetsu_brain::ambient::collect(workspace))
        } else {
            None
        };
    let ambient_suffix = ambient_ctx
        .as_ref()
        .map(kimetsu_brain::ambient::render_as_query_suffix)
        .filter(|s| !s.is_empty());

    match project::retrieve_benchmark_context_readonly_with_ambient(
        workspace,
        task,
        &dataset,
        task_slug.as_deref(),
        warm_policy,
        stage,
        budget_tokens,
        require_benchmark_memory,
        max_capsules,
        ambient_suffix.as_deref(),
    ) {
        Ok(context) => {
            let ok = context.required_ok;
            let error = if ok {
                None
            } else {
                Some("required exact-slug or generalized benchmark memory was not retrieved")
            };
            json!({
                "ok": ok,
                "error": error,
                "usage": {
                    "how_to_use": "Read playbook_markdown before broad exploration. The playbook prioritizes accepted semantic_operator and anti_pattern memories first, exact episodic run summaries as evidence, then repo snippets.",
                    "required_mode": "When require_benchmark_memory=true, ok=false means retrieval worked but no exact-slug or generalized benchmark memory was usable. Seed or record benchmark outcome memory before using strict required mode.",
                    "after_attempt": "Call kimetsu_benchmark_record_outcome with status, commands, pitfalls, verification, and optional generalized_memory so future benchmark runs retrieve a better playbook."
                },
                "dataset": context.dataset,
                "task": context.task,
                "task_slug": context.task_slug,
                "warm_policy": context.warm_policy.as_str(),
                "query": context.query,
                "ambient": ambient_ctx,
                "stage": context.stage,
                "budget_tokens": context.budget_tokens,
                "used_tokens": context.used_tokens,
                "capsule_count": context.capsule_count,
                "memory_capsule_count": context.memory_capsule_count,
                "benchmark_memory_count": context.benchmark_memory_count,
                "generalizable_memory_count": context.generalizable_memory_count,
                "episodic_memory_count": context.episodic_memory_count,
                "required_ok": context.required_ok,
                "playbook_markdown": context.playbook_markdown,
                "capsules": context.capsules,
                "excluded_count": context.excluded.len(),
                "excluded": context.excluded,
            })
        }
        Err(err) => brain_unavailable_json(workspace, &err.to_string()),
    }
}

fn kimetsu_benchmark_record_outcome(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let task = string_arg(arguments, "task")?;
    let dataset = optional_string_arg(arguments, "dataset")
        .unwrap_or_else(|| benchmark::DEFAULT_BENCHMARK_DATASET.to_string());
    let mode = optional_string_arg(arguments, "mode").unwrap_or_else(|| "unknown".to_string());
    let summary = optional_string_arg(arguments, "summary")
        .unwrap_or_else(|| "benchmark outcome recorded".to_string());
    let generalization = benchmark_memory_proposal_arg(arguments)?;
    let outcome = benchmark::BenchmarkOutcome {
        task,
        dataset,
        task_slug: optional_string_arg(arguments, "task_slug")
            .or_else(|| optional_string_arg(arguments, "slug")),
        warm_policy: optional_string_arg(arguments, "warm_policy")
            .or_else(|| optional_string_arg(arguments, "brain_warm_policy"))
            .as_deref()
            .and_then(benchmark::BenchmarkWarmPolicy::parse)
            .unwrap_or_default(),
        mode,
        passed: optional_bool_arg(arguments, "passed"),
        score: optional_f32_arg(arguments, "score"),
        error: optional_string_arg(arguments, "error"),
        summary,
        commands: string_list_arg(arguments, "commands"),
        pitfalls: string_list_arg(arguments, "pitfalls"),
        verify: string_list_arg(arguments, "verify"),
        cost_usd: optional_f32_arg(arguments, "cost_usd"),
        duration_seconds: optional_f32_arg(arguments, "duration_seconds"),
        generalization,
    };
    let recorded = project::record_benchmark_outcome(workspace, outcome)
        .map_err(|err| format!("kimetsu benchmark record outcome: {err}"))?;
    Ok(json!({
        "ok": true,
        "memory_id": recorded.memory_id,
        "task_slug": recorded.task_slug,
        "kind": recorded.kind.to_string(),
        "text": recorded.text,
        "proposal_id": recorded.proposal_id,
        "proposal_text": recorded.proposal_text,
        "proposal_status": if recorded.proposal_id.is_some() { Some("pending") } else { None },
        "usage": "Future kimetsu_benchmark_context calls can retrieve the episodic outcome memory. Generalized memory proposals remain pending until reviewed with kimetsu_brain_memory_accept or kimetsu_brain_memory_reject."
    }))
}

fn kimetsu_brain_memory_list(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let limit = u32_arg(arguments, "limit", 50, 1, 100);
    let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32;
    let memories = project::list_memories_with(
        workspace,
        project::ListOptions {
            limit,
            offset,
            scope: optional_string_arg(arguments, "scope"),
        },
    )
    .map_err(|err| format!("kimetsu brain memory list: {err}"))?;
    Ok(json!({
        "limit": limit,
        "offset": offset,
        "count": memories.len(),
        "memories": memories.iter().map(json_memory_row).collect::<Vec<_>>(),
        "usage": "Page with limit+offset. Use kimetsu_brain_memory_search to find by text, kimetsu_brain_memory_top for outcome-ranked trust signals, and kimetsu_brain_memory_invalidate to retire stale memories."
    }))
}

fn kimetsu_brain_memory_top(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let rows = project::list_memories_top(
        workspace,
        project::TopOptions {
            scope: optional_string_arg(arguments, "scope"),
            min_uses: u32_arg(arguments, "min_uses", 3, 1, 100),
            limit: u32_arg(arguments, "limit", 20, 1, 100),
        },
    )
    .map_err(|err| format!("kimetsu brain memory top: {err}"))?;
    Ok(json!({
        "memories": rows.iter().map(json_memory_row).collect::<Vec<_>>(),
        "usage": "Higher ratio means this memory has correlated with successful past runs; low or negative memories are candidates for invalidation."
    }))
}

fn kimetsu_brain_memory_add(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let scope = MemoryScope::from_str(&string_arg(arguments, "scope")?)?;
    let kind = MemoryKind::from_str(
        arguments
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("fact"),
    )?;
    let text = string_arg(arguments, "text")?;
    let memory_id = project::add_memory(workspace, scope, kind, &text)
        .map_err(|err| format!("kimetsu brain memory add: {err}"))?;
    Ok(json!({
        "memory_id": memory_id,
        "scope": scope.to_string(),
        "kind": kind.to_string(),
        "text": text,
    }))
}

fn kimetsu_brain_memory_proposals(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let proposals = project::list_proposals(
        workspace,
        project::ProposalFilter {
            scope: optional_string_arg(arguments, "scope"),
            kind: optional_string_arg(arguments, "kind"),
            from_run: optional_string_arg(arguments, "from_run"),
            min_confidence: optional_f32_arg(arguments, "min_confidence"),
            status: optional_string_arg(arguments, "status")
                .or_else(|| Some("pending".to_string())),
            limit: u32_arg(arguments, "limit", 50, 1, 200),
            offset: arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32,
        },
    )
    .map_err(|err| format!("kimetsu brain memory proposals: {err}"))?;
    Ok(json!({
        "proposals": proposals.iter().map(json_proposal_row).collect::<Vec<_>>(),
        "usage": "Accept only reusable preferences, conventions, commands, failure patterns, or facts. Reject task-specific or uncertain proposals."
    }))
}

fn kimetsu_brain_memory_accept(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let proposal_id = string_arg(arguments, "proposal_id")?;
    let memory_id = project::accept_proposal(
        workspace,
        &proposal_id,
        project::AcceptOverrides {
            scope: optional_string_arg(arguments, "scope"),
            confidence: optional_f32_arg(arguments, "confidence"),
        },
    )
    .map_err(|err| format!("kimetsu brain memory accept: {err}"))?;
    Ok(json!({
        "proposal_id": proposal_id,
        "memory_id": memory_id,
        "status": "accepted",
    }))
}

fn kimetsu_brain_memory_reject(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let proposal_id = string_arg(arguments, "proposal_id")?;
    let reason = optional_string_arg(arguments, "reason");
    project::reject_proposal(workspace, &proposal_id, reason.as_deref())
        .map_err(|err| format!("kimetsu brain memory reject: {err}"))?;
    Ok(json!({
        "proposal_id": proposal_id,
        "status": "rejected",
        "reason": reason,
    }))
}

fn kimetsu_brain_memory_invalidate(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let memory_id = string_arg(arguments, "memory_id")?;
    let reason = optional_string_arg(arguments, "reason");
    project::invalidate_memory(workspace, &memory_id, reason.as_deref())
        .map_err(|err| format!("kimetsu brain memory invalidate: {err}"))?;
    Ok(json!({
        "memory_id": memory_id,
        "status": "invalidated",
        "reason": reason,
    }))
}

/// v0.5.1: `kimetsu_brain_memory_blame` — per-run memory attribution
/// surfaced to the host harness. Same backend as the CLI
/// `kimetsu brain memory blame <run-id>`, JSON-shaped for Claude
/// Code / Codex to consume directly.
fn kimetsu_brain_memory_blame(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let run_id = string_arg(arguments, "run_id")?;
    let report = project::blame_run(workspace, run_id.trim())
        .map_err(|err| format!("kimetsu brain memory blame: {err}"))?;
    let json =
        serde_json::to_value(&report).map_err(|err| format!("serialize blame report: {err}"))?;
    Ok(json!({
        "ok": true,
        "usage": {
            "how_to_use": "Read `cited` first — those are the memories the model consciously used during the run. `silent_passengers` were retrieved into context but the model never called cite_memory on them, so they only earned the weak ±0.1 usefulness signal.",
            "next_steps": [
                "If a cited memory ended in a failed run, consider whether the memory is wrong — `kimetsu_brain_memory_invalidate` retires it.",
                "If a silent passenger consistently shows up and never gets cited, it's noise; consider invalidating or rewording it.",
                "Patterns across many runs are easier to see via `kimetsu_brain_memory_top` (usefulness ranking)."
            ]
        },
        "report": json,
    }))
}

/// v0.5.2: `kimetsu_brain_memory_conflicts` — list open conflict
/// hits across project + user brains. Same backend as the CLI
/// `kimetsu brain memory conflicts`, JSON-shaped for the host
/// harness. Read-only by design: actual resolution stays on the
/// CLI to keep the audit trail centralized.
fn kimetsu_brain_memory_conflicts(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
        .unwrap_or(50);
    let open = project::list_conflicts(workspace, limit)
        .map_err(|err| format!("kimetsu brain memory conflicts: {err}"))?;
    let conflicts =
        serde_json::to_value(&open).map_err(|err| format!("serialize conflicts: {err}"))?;
    Ok(json!({
        "ok": true,
        "usage": {
            "how_to_use": "Walk the list and look for pairs whose `new_text` directly contradicts `existing_text` (e.g. 'use anyhow' vs 'use thiserror'). For each true contradiction, the operator runs `kimetsu brain memory conflicts --resolve <conflict_id> <kept_new|kept_existing|kept_both>` from the CLI to settle it (resolution happens off this read-only MCP tool to keep the audit trail centralized).",
            "next_steps": [
                "If both memories are correct in different contexts, choose kept_both — neither is invalidated.",
                "If the new memory supersedes the existing one, choose kept_new — the existing memory is invalidated.",
                "If the existing memory is the authoritative version, choose kept_existing — the new memory is invalidated.",
                "If the list looks suspiciously empty: a) you may be on the lean build where conflict detection is a no-op, or b) no contradictory writes have happened yet."
            ]
        },
        "limit": limit,
        "open_count": open.len(),
        "conflicts": conflicts,
    }))
}

fn kimetsu_brain_ingest_repo(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let path = optional_string_arg(arguments, "path")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| workspace.to_path_buf());
    let summary =
        project::ingest_repo(&path).map_err(|err| format!("kimetsu brain ingest repo: {err}"))?;
    Ok(json!({
        "repo_root": summary.repo_root,
        "indexed_files": summary.indexed_files,
        "skipped_files": summary.skipped_files,
        "manifests": summary.manifests,
        "usage": "After ingest, call kimetsu_brain_context with the task query to retrieve repo-aware capsules."
    }))
}

/// v0.8: list the curated built-in embedding models and the active id.
fn kimetsu_brain_model_list(workspace: &Path) -> Result<Value, String> {
    use kimetsu_brain::embeddings::{BUILTIN_MODELS, resolve_embedder_id};
    let config_model = project::load_config(
        &kimetsu_core::paths::ProjectPaths::discover(workspace)
            .map_err(|err| format!("discover workspace: {err}"))?,
    )
    .ok()
    .map(|cfg| cfg.embedder.model);
    let active = resolve_embedder_id(config_model.as_deref());
    let models: Vec<Value> = BUILTIN_MODELS
        .iter()
        .map(|(id, dim, blurb)| {
            json!({ "id": id, "dim": dim, "description": blurb, "active": *id == active })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "active": active,
        "configured": config_model,
        "models": models,
        "usage": "Call kimetsu_brain_model_set to change the model. Note: a switch only affects new embeddings; restart the MCP server and run kimetsu_brain_reindex (or `kimetsu brain reindex --force` from the CLI) to re-embed existing memories."
    }))
}

/// v0.8: change the embedding model. Records it in project.toml and
/// (unless `reindex:false`) re-embeds the corpus in-process with a
/// FRESH embedder for the new model — independent of the model this
/// server loaded at startup. Note: the server's *retrieval* query
/// embedder is a process-static singleton, so semantic retrieval in
/// THIS session keeps using the old model (cross-model rows safely fall
/// back to FTS) until the server restarts; the stored embeddings are
/// already migrated, so a restart fully activates the new model.
fn kimetsu_brain_model_set(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    use kimetsu_brain::embeddings::resolve_embedder_id;
    let id = string_arg(arguments, "id")?;
    // Validate against known aliases so a typo doesn't silently fall
    // back to the default model.
    if !is_known_embedder_alias(&id) {
        return Err(format!(
            "unknown embedder id `{id}`. Call kimetsu_brain_model_list for options."
        ));
    }
    let canonical = resolve_embedder_id(Some(&id));
    let paths = kimetsu_core::paths::ProjectPaths::discover(workspace)
        .map_err(|err| format!("discover workspace: {err}"))?;
    let mut config = project::load_config(&paths).map_err(|err| format!("load config: {err}"))?;
    let previous = config.embedder.model.clone();
    config.embedder.model = canonical.to_string();
    let toml = config
        .to_toml()
        .map_err(|err| format!("serialize config: {err}"))?;
    std::fs::write(&paths.project_toml, toml)
        .map_err(|err| format!("write project.toml: {err}"))?;

    let do_reindex = arguments
        .get("reindex")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !do_reindex {
        return Ok(json!({
            "ok": true,
            "model": canonical,
            "previous": previous,
            "reindexed": false,
            "note": "Recorded in project.toml. Passed reindex:false, so existing memories keep their old embeddings until you run kimetsu_brain_reindex or `kimetsu brain reindex --force`."
        }));
    }

    // Re-embed with a fresh embedder for the NEW model (not the server's
    // cached default). The candidate predicate re-embeds every row whose
    // embedding_model != the new model — i.e. all of them.
    let embedder = kimetsu_brain::embeddings::open_embedder_for_model(canonical);
    let report = kimetsu_brain::reindex::reindex_all_with_embedder(
        workspace,
        kimetsu_brain::reindex::ReindexOptions {
            scope: kimetsu_brain::reindex::ReindexScope::All,
            dry_run: false,
            force: false,
            limit: None,
        },
        embedder.as_ref(),
    )
    .map_err(|err| format!("reindex after model set: {err}"))?;
    Ok(json!({
        "ok": true,
        "model": canonical,
        "previous": previous,
        "reindexed": !report.embedder_noop,
        "updated": report.updated_total(),
        "embedder_noop": report.embedder_noop,
        "note": if report.embedder_noop {
            "Recorded, but this is a lean (no-embeddings) build so no vectors were produced. Reinstall with `--features embeddings` to enable semantic retrieval."
        } else {
            "Recorded and existing memories re-embedded with the new model. Restart the MCP server so its retrieval query embedder also switches (until then, retrieval falls back to FTS for the migrated rows)."
        }
    }))
}

fn is_known_embedder_alias(id: &str) -> bool {
    matches!(
        id.trim().to_ascii_lowercase().as_str(),
        "default"
            | "bge-small"
            | "bge-small-en-v1.5"
            | "bge-m3"
            | "m3"
            | "jina-code"
            | "jina-v2-base-code"
            | "jina-embeddings-v2-base-code"
    )
}

/// v0.8: backfill stale/missing embeddings using the server's CURRENT
/// embedder (the one loaded at startup). Useful after adding memories;
/// to switch models, see kimetsu_brain_model_set.
fn kimetsu_brain_reindex(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let scope = kimetsu_brain::reindex::ReindexScope::parse(
        &optional_string_arg(arguments, "scope").unwrap_or_else(|| "all".to_string()),
    )?;
    let report = kimetsu_brain::reindex::reindex_all(
        workspace,
        kimetsu_brain::reindex::ReindexOptions {
            scope,
            dry_run: arguments
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            force: arguments
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            limit: arguments
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
        },
    )
    .map_err(|err| format!("kimetsu brain reindex: {err}"))?;
    Ok(json!({
        "ok": true,
        "model": report.embedder_model_id,
        "embedder_noop": report.embedder_noop,
        "candidates": report.candidates_total(),
        "updated": report.updated_total(),
    }))
}

/// v0.8: full-text search over memory text for navigating the corpus.
fn kimetsu_brain_memory_search(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let query = string_arg(arguments, "query")?;
    let limit = u32_arg(arguments, "limit", 20, 1, 100);
    let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32;
    let hits = project::search_memories(
        workspace,
        &query,
        limit,
        offset,
        optional_string_arg(arguments, "kind").as_deref(),
        optional_string_arg(arguments, "scope").as_deref(),
    )
    .map_err(|err| format!("kimetsu brain memory search: {err}"))?;
    Ok(json!({
        "ok": true,
        "query": query,
        "limit": limit,
        "offset": offset,
        "count": hits.len(),
        "results": hits.iter().map(|h| json!({
            "memory_id": h.memory_id,
            "scope": h.scope,
            "kind": h.kind,
            "text": h.text,
            "rank": h.rank,
        })).collect::<Vec<_>>(),
        "usage": "Page with limit+offset. Filter by kind (failure_pattern/command/convention/preference/fact) or scope (global_user/project/repo/run)."
    }))
}

/// v0.8: settle an open memory conflict from inside the agent.
fn kimetsu_brain_conflict_resolve(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let conflict_id = string_arg(arguments, "conflict_id")?;
    let resolution = string_arg(arguments, "resolution")?;
    if !matches!(
        resolution.as_str(),
        "kept_new" | "kept_existing" | "kept_both"
    ) {
        return Err("`resolution` must be one of: kept_new, kept_existing, kept_both".to_string());
    }
    let resolved = project::resolve_conflict(workspace, &conflict_id, &resolution)
        .map_err(|err| format!("kimetsu brain conflict resolve: {err}"))?;
    Ok(json!({
        "ok": resolved,
        "conflict_id": conflict_id,
        "resolution": resolution,
        "resolved": resolved,
        "usage": if resolved { "Conflict settled. kept_new/kept_existing invalidates the losing memory; kept_both keeps both." } else { "No open conflict with that id (already resolved or unknown)." }
    }))
}

/// v0.8: prune net-negative memories. Defaults to a dry run (apply:false).
fn kimetsu_brain_prune(workspace: &Path, arguments: &Value) -> Result<Value, String> {
    let apply = arguments
        .get("apply")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = project::prune_low_usefulness(
        workspace,
        project::PruneOptions {
            scope: optional_string_arg(arguments, "scope"),
            min_uses: u32_arg(arguments, "min_uses", 3, 1, 1000),
            max_ratio: optional_f32_arg(arguments, "max_ratio").unwrap_or(0.0),
            apply,
        },
    )
    .map_err(|err| format!("kimetsu brain prune: {err}"))?;
    Ok(json!({
        "ok": true,
        "apply": apply,
        "candidate_count": summary.candidates.len(),
        "invalidated": summary.invalidated,
        "failed": summary.failed,
        "candidates": summary.candidates.iter().map(|c| json!({
            "memory_id": c.memory_id,
            "scope": c.scope,
            "kind": c.kind,
            "text": c.text,
            "use_count": c.use_count,
            "usefulness_score": c.usefulness_score,
        })).collect::<Vec<_>>(),
        "usage": "This is a dry run unless apply:true. Candidates are memories with usefulness_score/use_count <= max_ratio and use_count >= min_uses."
    }))
}

/// v0.8: read-only view of the project.toml config.
fn kimetsu_brain_config_show(workspace: &Path) -> Result<Value, String> {
    let raw = project::config_text(workspace)
        .map_err(|err| format!("kimetsu brain config show: {err}"))?;
    let parsed: Value = toml::from_str(&raw).unwrap_or(Value::Null);
    Ok(json!({
        "ok": true,
        "raw": raw,
        "config": parsed,
    }))
}

fn brain_unavailable_json(workspace: &Path, error: &str) -> Value {
    json!({
        "initialized": false,
        "workspace": workspace,
        "error": error,
        "setup_hints": [
            "Run `kimetsu init` in the workspace to create .kimetsu/project.toml and brain.db.",
            "Run `kimetsu brain ingest-repo .` or call kimetsu_brain_ingest_repo after init to index repo files.",
            "Add or accept memories before expecting memory capsules from kimetsu_brain_context or benchmark memories from kimetsu_benchmark_context."
        ]
    })
}

fn json_memory_row(memory: &project::MemoryRow) -> Value {
    let usefulness_ratio = if memory.use_count > 0 {
        Some(memory.usefulness_score / memory.use_count as f32)
    } else {
        None
    };
    json!({
        "memory_id": memory.memory_id,
        "scope": memory.scope,
        "kind": memory.kind,
        "text": memory.text,
        "confidence": memory.confidence,
        "use_count": memory.use_count,
        "usefulness_score": memory.usefulness_score,
        "usefulness_ratio": usefulness_ratio,
    })
}

fn json_proposal_row(proposal: &project::ProposalRow) -> Value {
    json!({
        "proposal_id": proposal.proposal_id,
        "run_id": proposal.run_id,
        "scope": proposal.scope,
        "kind": proposal.kind,
        "text": proposal.text,
        "rationale": proposal.rationale,
        "proposed_confidence": proposal.proposed_confidence,
        "status": proposal.status,
        "decided_reason": proposal.decided_reason,
    })
}

fn string_arg(arguments: &Value, name: &str) -> Result<String, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing `{name}`"))
}

fn optional_string_arg(arguments: &Value, name: &str) -> Option<String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_f32_arg(arguments: &Value, name: &str) -> Option<f32> {
    arguments.get(name).and_then(|value| match value {
        Value::Number(number) => number.as_f64().map(|value| value as f32),
        Value::String(text) => text.parse::<f32>().ok(),
        _ => None,
    })
}

fn optional_bool_arg(arguments: &Value, name: &str) -> Option<bool> {
    arguments.get(name).and_then(|value| match value {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn bool_arg(arguments: &Value, name: &str, default: bool) -> bool {
    optional_bool_arg(arguments, name).unwrap_or(default)
}

fn string_list_arg(arguments: &Value, name: &str) -> Vec<String> {
    match arguments.get(name) {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        Some(Value::String(text)) => text
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn first_string_list_arg(arguments: &Value, names: &[&str]) -> Vec<String> {
    for name in names {
        let values = string_list_arg(arguments, name);
        if !values.is_empty() {
            return values;
        }
    }
    Vec::new()
}

fn benchmark_memory_proposal_arg(
    arguments: &Value,
) -> Result<Option<benchmark::BenchmarkMemoryProposal>, String> {
    let anti_pattern_text = optional_string_arg(arguments, "anti_pattern_memory");
    let semantic_text = optional_string_arg(arguments, "semantic_memory")
        .or_else(|| optional_string_arg(arguments, "operator_memory"));
    let generalized_text = optional_string_arg(arguments, "generalized_memory")
        .or_else(|| optional_string_arg(arguments, "generalization"));

    let (text, implied_role) = if let Some(text) = anti_pattern_text {
        (text, benchmark::BenchmarkMemoryRole::AntiPattern)
    } else if let Some(text) = semantic_text {
        (text, benchmark::BenchmarkMemoryRole::SemanticOperator)
    } else if let Some(text) = generalized_text {
        (text, benchmark::BenchmarkMemoryRole::SemanticOperator)
    } else {
        return Ok(None);
    };

    let role = optional_string_arg(arguments, "memory_role")
        .or_else(|| optional_string_arg(arguments, "generalized_memory_role"))
        .or_else(|| optional_string_arg(arguments, "generalization_role"))
        .map(|value| {
            benchmark::BenchmarkMemoryRole::parse(&value)
                .ok_or_else(|| format!("invalid benchmark memory role `{value}`"))
        })
        .transpose()?
        .unwrap_or(implied_role);

    if !role.is_generalizable() {
        return Err(
            "`generalized_memory` requires memory_role=semantic_operator or anti_pattern"
                .to_string(),
        );
    }

    let confidence = optional_f32_arg(arguments, "generalization_confidence")
        .or_else(|| optional_f32_arg(arguments, "memory_confidence"))
        .or_else(|| optional_f32_arg(arguments, "confidence"))
        .unwrap_or(0.7);
    let rationale = optional_string_arg(arguments, "generalization_rationale")
        .or_else(|| optional_string_arg(arguments, "rationale"))
        .unwrap_or_else(|| {
            "Candidate reusable benchmark lesson; keep pending for human review.".to_string()
        });

    Ok(Some(benchmark::BenchmarkMemoryProposal {
        role,
        text,
        task_family: optional_string_arg(arguments, "task_family"),
        applies_to: first_string_list_arg(arguments, &["applies_to", "applies"]),
        does_not_apply_to: first_string_list_arg(
            arguments,
            &["does_not_apply_to", "not_applies_to", "non_examples"],
        ),
        evidence_for: first_string_list_arg(arguments, &["evidence_for", "supporting_evidence"]),
        evidence_against: first_string_list_arg(
            arguments,
            &["evidence_against", "counter_evidence"],
        ),
        rationale,
        confidence,
    }))
}

fn u32_arg(arguments: &Value, name: &str, default: u32, min: u32, max: u32) -> u32 {
    let value = arguments
        .get(name)
        .and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse::<u64>().ok(),
            _ => None,
        })
        .unwrap_or(default as u64);
    (value as u32).clamp(min, max)
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "kimetsu_brain_status",
            "description": BRAIN_STATUS_DESCRIPTION,
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kimetsu_brain_context",
            "description": BRAIN_CONTEXT_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "stage": {
                        "type": "string",
                        "enum": ["localization", "patch_plan", "implementation", "verification", "review"]
                    },
                    "budget_tokens": { "type": "integer", "minimum": 500, "maximum": 30000 },
                    "min_score": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "Skip threshold — if the best capsule scores below this, return empty (zero tokens injected). Default 0.15." },
                    "max_capsules": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Hard cap on returned capsules. Default 3." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Domain-hint tags. Capsules whose text contains any of these get a 1.4× score boost." },
                    "prefer_roles": { "type": "array", "items": { "type": "string" }, "description": "Boost capsules whose kind matches (e.g. [\"semantic_operator\",\"anti_pattern\"] for bench use)." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "kimetsu_brain_record",
            "description": "Record a concrete, reusable lesson into the brain. Call after solving a non-obvious problem that required real effort. Do NOT call for trivial or well-known knowledge. High-confidence lessons (≥0.7) are accepted immediately; low-confidence go to pending proposals.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "lesson": { "type": "string", "description": "The generalizable rule in concrete, actionable form." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "2-5 domain keywords (e.g. [\"rust\",\"linker\",\"windows\"])." },
                    "context": { "type": "string", "description": "Optional: what problem triggered this lesson." },
                    "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0, "description": "How confident you are this generalizes. Default 0.8." },
                    "kind": {
                        "type": "string",
                        "enum": ["semantic_operator", "anti_pattern", "convention"],
                        "description": "semantic_operator for positive rules, anti_pattern for things to avoid, convention for style/project norms. Default semantic_operator."
                    }
                },
                "required": ["lesson", "tags"]
            }
        },
        {
            "name": "kimetsu_benchmark_context",
            "description": BENCHMARK_CONTEXT_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "query": { "type": "string", "description": "Alias for task for compatibility with generic brain helpers." },
                    "dataset": { "type": "string", "default": "terminal-bench/terminal-bench-2" },
                    "task_slug": { "type": "string", "description": "Optional Terminal-Bench slug such as compile-compcert. Kimetsu detects it from task text when omitted." },
                    "warm_policy": {
                        "type": "string",
                        "enum": ["cold_brain", "reactive_warm", "full_warm"],
                        "description": "cold_brain excludes memories; reactive_warm makes Kimetsu available without requiring task memory; full_warm is the pre-task injected playbook condition."
                    },
                    "stage": {
                        "type": "string",
                        "enum": ["localization", "patch_plan", "implementation", "verification", "review", "harbor"]
                    },
                    "budget_tokens": { "type": "integer", "minimum": 500, "maximum": 30000 },
                    "max_capsules": { "type": "integer", "minimum": 1, "maximum": 20 },
                    "require_benchmark_memory": { "type": "boolean", "description": "When true, ok=false unless at least one exact-slug episodic memory or generalized semantic/anti-pattern benchmark memory is in the playbook." }
                },
                "required": ["task"]
            }
        },
        {
            "name": "kimetsu_benchmark_record_outcome",
            "description": BENCHMARK_RECORD_OUTCOME_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "dataset": { "type": "string", "default": "terminal-bench/terminal-bench-2" },
                    "task_slug": { "type": "string" },
                    "warm_policy": { "type": "string", "enum": ["cold_brain", "reactive_warm", "full_warm"] },
                    "mode": { "type": "string", "description": "Benchmark mode, e.g. required-kimetsu, optional-kimetsu, no-kimetsu, claude-code-brain." },
                    "passed": { "type": "boolean" },
                    "score": { "type": "number" },
                    "error": { "type": "string" },
                    "summary": { "type": "string" },
                    "commands": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "pitfalls": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "verify": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "generalized_memory": { "type": "string", "description": "Optional reusable lesson to propose for human review. Use for transferable tactics or warnings, not exact run summaries." },
                    "semantic_memory": { "type": "string", "description": "Alias for generalized_memory with memory_role=semantic_operator." },
                    "anti_pattern_memory": { "type": "string", "description": "Alias for generalized_memory with memory_role=anti_pattern." },
                    "memory_role": {
                        "type": "string",
                        "enum": ["semantic_operator", "anti_pattern"],
                        "description": "Role for generalized_memory. Exact task/run summaries are recorded automatically as memory_role=episodic."
                    },
                    "task_family": { "type": "string", "description": "Reusable family the generalized memory applies to, e.g. generated-artifact-verification." },
                    "applies_to": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "does_not_apply_to": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "evidence_for": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "evidence_against": { "oneOf": [{ "type": "array", "items": { "type": "string" } }, { "type": "string" }] },
                    "generalization_rationale": { "type": "string" },
                    "generalization_confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "cost_usd": { "type": "number" },
                    "duration_seconds": { "type": "number" }
                },
                "required": ["task"]
            }
        },
        {
            "name": "kimetsu_brain_memory_list",
            "description": BRAIN_MEMORY_LIST_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }
        },
        {
            "name": "kimetsu_brain_memory_top",
            "description": BRAIN_MEMORY_TOP_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["global_user", "project", "repo", "run"] },
                    "min_uses": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }
        },
        {
            "name": "kimetsu_brain_memory_add",
            "description": BRAIN_MEMORY_ADD_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["global_user", "project", "repo", "run"] },
                    "kind": { "type": "string", "enum": ["preference", "convention", "command", "failure_pattern", "fact"] },
                    "text": { "type": "string" }
                },
                "required": ["scope", "text"]
            }
        },
        {
            "name": "kimetsu_brain_memory_proposals",
            "description": BRAIN_MEMORY_PROPOSALS_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["global_user", "project", "repo", "run"] },
                    "kind": { "type": "string", "enum": ["preference", "convention", "command", "failure_pattern", "fact"] },
                    "from_run": { "type": "string" },
                    "min_confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "status": { "type": "string", "enum": ["pending", "accepted", "rejected", "any"] },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
                }
            }
        },
        {
            "name": "kimetsu_brain_memory_accept",
            "description": BRAIN_MEMORY_ACCEPT_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "proposal_id": { "type": "string" },
                    "scope": { "type": "string", "enum": ["global_user", "project", "repo", "run"] },
                    "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
                },
                "required": ["proposal_id"]
            }
        },
        {
            "name": "kimetsu_brain_memory_reject",
            "description": BRAIN_MEMORY_REJECT_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "proposal_id": { "type": "string" },
                    "reason": { "type": "string" }
                },
                "required": ["proposal_id"]
            }
        },
        {
            "name": "kimetsu_brain_memory_invalidate",
            "description": BRAIN_MEMORY_INVALIDATE_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string" },
                    "reason": { "type": "string" }
                },
                "required": ["memory_id"]
            }
        },
        {
            "name": "kimetsu_brain_memory_blame",
            "description": BRAIN_MEMORY_BLAME_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": { "type": "string" }
                },
                "required": ["run_id"]
            }
        },
        {
            "name": "kimetsu_brain_memory_conflicts",
            "description": BRAIN_MEMORY_CONFLICTS_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
                }
            }
        },
        {
            "name": "kimetsu_brain_ingest_repo",
            "description": BRAIN_INGEST_REPO_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }
        },
        {
            "name": "kimetsu_bridge_status",
            "description": BRIDGE_STATUS_DESCRIPTION,
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kimetsu_skills_search",
            "description": SKILLS_SEARCH_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string" } }
            }
        },
        {
            "name": "kimetsu_bridge_import",
            "description": BRIDGE_IMPORT_DESCRIPTION,
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
            "description": BRIDGE_EXPORT_DESCRIPTION,
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
            "description": BRIDGE_SYNC_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": { "force": { "type": "boolean" } }
            }
        },
        {
            "name": "kimetsu_plugin_install",
            "description": PLUGIN_INSTALL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "enum": ["claude-code", "codex"] },
                    "scope": {
                        "type": "string",
                        "enum": ["workspace", "global"],
                        "description": "workspace (default) installs into this workspace's .claude/.codex; global installs into ~/.claude(.json) and ~/.codex for all sessions."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["optional", "required"],
                        "description": "optional recommends Kimetsu brain first; required tells the host harness to block non-trivial work until Kimetsu context is available or explicitly waived. Benchmark guidance prefers kimetsu_benchmark_context."
                    },
                    "force": { "type": "boolean" },
                    "proactive": { "type": "boolean", "description": "Default true. Set false to skip the proactive PreToolUse/PostToolUse Bash hooks (mid-work recall); UserPromptSubmit + Stop still install." }
                },
                "required": ["target"]
            }
        },
        {
            "name": "kimetsu_brain_model_list",
            "description": "List the curated built-in embedding models and the active one. The user can switch models from here (kimetsu_brain_model_set).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kimetsu_brain_model_set",
            "description": "Set the brain's embedding model (a built-in id from kimetsu_brain_model_list). Records it in project.toml and (unless reindex:false) re-embeds the corpus with the new model in-process. The server's retrieval query embedder is fixed until restart, so semantic retrieval this session falls back to FTS for migrated rows; restart to fully activate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Built-in model id, e.g. bge-small-en-v1.5, bge-m3, jina-v2-base-code." },
                    "reindex": { "type": "boolean", "description": "Default true. Re-embed existing memories with the new model now. Set false to record the id only." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "kimetsu_brain_reindex",
            "description": "Backfill stale/missing embeddings using the server's current embedder. Run after adding memories. To change models, use kimetsu_brain_model_set.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["project", "user", "all"] },
                    "dry_run": { "type": "boolean" },
                    "force": { "type": "boolean" },
                    "limit": { "type": "integer", "minimum": 1 }
                }
            }
        },
        {
            "name": "kimetsu_brain_memory_search",
            "description": "Full-text search over memory text. Page with limit+offset; filter by kind or scope. Use this to navigate the memory corpus.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "kind": { "type": "string", "enum": ["preference", "convention", "command", "failure_pattern", "fact"] },
                    "scope": { "type": "string", "enum": ["global_user", "project", "repo", "run"] }
                },
                "required": ["query"]
            }
        },
        {
            "name": "kimetsu_brain_conflict_resolve",
            "description": "Settle an open memory conflict (from kimetsu_brain_memory_conflicts) by id. kept_new/kept_existing invalidates the losing memory; kept_both keeps both.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "conflict_id": { "type": "string" },
                    "resolution": { "type": "string", "enum": ["kept_new", "kept_existing", "kept_both"] }
                },
                "required": ["conflict_id", "resolution"]
            }
        },
        {
            "name": "kimetsu_brain_prune",
            "description": "List (or with apply:true, invalidate) net-negative memories whose usefulness ratio is at or below max_ratio. Defaults to a dry run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["global_user", "project", "repo", "run"] },
                    "min_uses": { "type": "integer", "minimum": 1 },
                    "max_ratio": { "type": "number" },
                    "apply": { "type": "boolean" }
                }
            }
        },
        {
            "name": "kimetsu_brain_config_show",
            "description": "Read the project.toml config (raw + parsed), including the active embedder, broker weights, and run limits.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kimetsu_brain_insights",
            "description": "Brain effectiveness analytics: retrieval hit-rate, citation rate, proposal acceptance, usefulness trend, harvest yield, token economy. Use to see whether the brain is helping and to tune it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "last_n_runs": { "type": "integer", "minimum": 1, "description": "Number of most-recent runs to include in the rolling window. Default 50." },
                    "since": { "type": "string", "description": "ISO-8601 lower bound on run timestamps. When set, overrides last_n_runs." },
                    "top": { "type": "integer", "minimum": 1, "description": "How many items to include in ranked lists (top-useful, prune-candidates). Default 10." }
                }
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        assert!(
            result["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["name"].as_str() == Some("kimetsu_brain_context") })
        );
        assert!(
            result["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["name"].as_str() == Some("kimetsu_benchmark_context") })
        );
        let status = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"].as_str() == Some("kimetsu_bridge_status"))
            .unwrap();
        assert!(
            status["description"]
                .as_str()
                .unwrap()
                .contains("portable skills")
        );
        let plugin_install = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"].as_str() == Some("kimetsu_plugin_install"))
            .unwrap();
        assert!(
            plugin_install["inputSchema"]["properties"]["mode"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("required"))
        );
    }

    #[test]
    fn initialize_explains_kimetsu_workflow() {
        let result = handle_mcp_method(
            "initialize",
            json!({}),
            Path::new("."),
            &SkillConfig::default(),
        )
        .expect("initialize");
        assert!(
            result["instructions"]
                .as_str()
                .unwrap()
                .contains("Recommended workflow")
        );
    }

    #[test]
    fn brain_status_reports_missing_project_without_error() {
        let root = temp_root("kimetsu-mcp-no-brain");
        fs::create_dir_all(&root).expect("create temp root");
        let result = call_tool(
            "kimetsu_brain_status",
            json!({}),
            &root,
            &SkillConfig::default(),
        )
        .expect("brain status");
        assert_eq!(result["initialized"].as_bool(), Some(false));
        fs::remove_dir_all(root).expect("remove temp root");
    }

    #[test]
    fn brain_insights_appears_in_tool_definitions() {
        let result = handle_mcp_method(
            "tools/list",
            json!({}),
            Path::new("."),
            &SkillConfig::default(),
        )
        .expect("tools/list");
        let tools = result["tools"].as_array().unwrap();
        let insights_tool = tools
            .iter()
            .find(|tool| tool["name"].as_str() == Some("kimetsu_brain_insights"))
            .expect("kimetsu_brain_insights must be in tool_definitions");
        // Description must mention analytics.
        assert!(
            insights_tool["description"]
                .as_str()
                .unwrap_or("")
                .contains("analytics"),
            "kimetsu_brain_insights description should contain 'analytics'"
        );
        // Schema must accept optional last_n_runs, since, top.
        let props = &insights_tool["inputSchema"]["properties"];
        assert!(
            props.get("last_n_runs").is_some(),
            "schema must have last_n_runs"
        );
        assert!(props.get("since").is_some(), "schema must have since");
        assert!(props.get("top").is_some(), "schema must have top");
    }

    #[test]
    fn brain_insights_reports_missing_project_without_error() {
        let root = temp_root("kimetsu-mcp-insights-no-brain");
        fs::create_dir_all(&root).expect("create temp root");
        let result = call_tool(
            "kimetsu_brain_insights",
            json!({}),
            &root,
            &SkillConfig::default(),
        )
        .expect("brain insights call");
        // No brain — must return initialized:false, not panic.
        assert_eq!(result["initialized"].as_bool(), Some(false));
        fs::remove_dir_all(root).expect("remove temp root");
    }

    #[test]
    fn brain_insights_returns_well_formed_report() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = temp_root("kimetsu-mcp-insights-brain");
            fs::create_dir_all(&root).expect("create temp root");
            project::init_project(&root, false).expect("init project");
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Convention,
                "insights mcp test fixture memory",
            )
            .expect("add memory");

            let result = call_tool(
                "kimetsu_brain_insights",
                json!({ "last_n_runs": 50, "top": 5 }),
                &root,
                &SkillConfig::default(),
            )
            .expect("brain insights call");

            assert_eq!(result["ok"].as_bool(), Some(true), "ok must be true");
            // The report must have the top-level sections.
            let report = &result["report"];
            assert!(
                report.get("retrieval").is_some(),
                "report.retrieval missing"
            );
            assert!(report.get("citation").is_some(), "report.citation missing");
            assert!(
                report.get("proposals").is_some(),
                "report.proposals missing"
            );
            assert!(
                report.get("usefulness").is_some(),
                "report.usefulness missing"
            );
            assert!(report.get("harvest").is_some(), "report.harvest missing");
            assert!(report.get("corpus").is_some(), "report.corpus missing");
            assert!(
                report.get("token_economy").is_some(),
                "report.token_economy missing"
            );
            // interpretation string must be present and non-empty.
            let interp = result["interpretation"].as_str().unwrap_or("");
            assert!(!interp.is_empty(), "interpretation must be non-empty");
            // hit_rate is None (C7 not landed) — JSON null in the report.
            assert!(
                report["retrieval"]["hit_rate"].is_null(),
                "hit_rate must be null (C7 not yet landed)"
            );
            fs::remove_dir_all(root).expect("remove temp root");
        });
    }

    #[test]
    fn brain_context_returns_memory_capsules() {
        let root = temp_root("kimetsu-mcp-brain");
        fs::create_dir_all(&root).expect("create temp root");
        project::init_project(&root, false).expect("init project");
        project::add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "Use ripgrep before broad file reads.",
        )
        .expect("add memory");

        let result = call_tool(
            "kimetsu_brain_context",
            json!({
                "query": "search files with ripgrep before reading",
                "stage": "localization",
                "budget_tokens": 4000
            }),
            &root,
            &SkillConfig::default(),
        )
        .expect("brain context");

        assert_eq!(result["ok"].as_bool(), Some(true));
        let capsules = result["capsules"].as_array().expect("capsules");
        assert!(
            capsules.iter().any(|capsule| {
                capsule["expansion_handle"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("memory:")
            }),
            "expected a memory capsule: {capsules:?}"
        );
        fs::remove_dir_all(root).expect("remove temp root");
    }

    /// v1.0.0: `brain_context_tool` with a `StubReranker` reorders and caps
    /// capsules. Seeds two memories — one semantically close to the query, one
    /// unrelated — and confirms the reranker places the closer one first and
    /// respects `max_capsules`.
    #[test]
    fn brain_context_tool_with_stub_reranker_reorders_and_caps() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = temp_root("kimetsu-mcp-rerank");
            fs::create_dir_all(&root).expect("create temp root");
            project::init_project(&root, false).expect("init project");

            // High-relevance memory: shares many tokens with the query.
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Convention,
                "Use ripgrep for fast file search before broad reads",
            )
            .expect("add high-relevance memory");

            // Low-relevance memory: unrelated tokens.
            project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Convention,
                "Quibblefrotz wobblecache unrelated zephyrqux datum",
            )
            .expect("add low-relevance memory");

            let rr = kimetsu_brain::embeddings::StubReranker;
            let args = json!({
                "query": "search files ripgrep before reading",
                "stage": "localization",
                "min_score": 0.0,
                "max_capsules": 1,
            });

            let result =
                brain_context_tool(&root, &args, Some(&rr)).expect("brain_context_tool");

            assert_eq!(result["ok"].as_bool(), Some(true), "ok false: {result}");
            // The reranker caps at max_capsules=1.
            let capsules = result["capsules"].as_array().expect("capsules array");
            assert!(
                capsules.len() <= 1,
                "reranker must cap to max_capsules=1, got {}: {result}",
                capsules.len()
            );
            // The top capsule (if present) must be the ripgrep memory
            // (higher token overlap with the query).
            if let Some(top) = capsules.first() {
                let summary = top["summary"].as_str().unwrap_or("");
                assert!(
                    summary.contains("ripgrep"),
                    "StubReranker must rank the ripgrep memory first: {summary}"
                );
            }
            fs::remove_dir_all(root).expect("remove temp root");
        });
    }

    #[test]
    fn benchmark_context_returns_playbook_and_enforces_task_memory() {
        // v0.4.1: this test writes a GlobalUser benchmark memory and
        // expects to read exactly one matching capsule back. The
        // default user-brain routing would land that write in the
        // shared `~/.kimetsu/brain.db` and the host machine's actual
        // user-brain rows would leak into `benchmark_memory_count`.
        // Disable for this test; the benchmark MCP path is exercised
        // against both DBs in the BrainSession integration tests.
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = temp_root("kimetsu-mcp-benchmark");
            fs::create_dir_all(&root).expect("create temp root");
            project::init_project(&root, false).expect("init project");
            project::add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Command,
                "[terminal-bench:compile-compcert] Redirect make output to /tmp/build.log, patch the failing configure check, then rerun the benchmark verifier.",
            )
            .expect("add benchmark memory");

            let result = call_tool(
                "kimetsu_benchmark_context",
                json!({
                    "task": "compile-compcert build and test CompCert",
                    "dataset": "terminal-bench/terminal-bench-2",
                    "warm_policy": "full_warm",
                    "require_benchmark_memory": true,
                    "budget_tokens": 4000
                }),
                &root,
                &SkillConfig::default(),
            )
            .expect("benchmark context");

            assert_eq!(result["ok"].as_bool(), Some(true));
            assert_eq!(result["task_slug"].as_str(), Some("compile-compcert"));
            assert_eq!(result["warm_policy"].as_str(), Some("full_warm"));
            assert_eq!(result["benchmark_memory_count"].as_u64(), Some(1));
            assert!(
                result["playbook_markdown"]
                    .as_str()
                    .unwrap()
                    .contains("Kimetsu Benchmark Playbook")
            );
            fs::remove_dir_all(root).expect("remove temp root");
        });
    }

    #[test]
    fn benchmark_context_required_reports_missing_task_memory() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = temp_root("kimetsu-mcp-benchmark-empty");
            fs::create_dir_all(&root).expect("create temp root");
            project::init_project(&root, false).expect("init project");

            let result = call_tool(
                "kimetsu_benchmark_context",
                json!({
                    "task": "compile-compcert build and test CompCert",
                    "warm_policy": "full_warm",
                    "require_benchmark_memory": true,
                    "budget_tokens": 4000
                }),
                &root,
                &SkillConfig::default(),
            )
            .expect("benchmark context");

            assert_eq!(result["ok"].as_bool(), Some(false));
            assert_eq!(result["required_ok"].as_bool(), Some(false));
            fs::remove_dir_all(root).expect("remove temp root");
        });
    }

    #[test]
    fn benchmark_record_outcome_writes_retrievable_memory() {
        let root = temp_root("kimetsu-mcp-benchmark-record");
        fs::create_dir_all(&root).expect("create temp root");
        project::init_project(&root, false).expect("init project");

        let result = call_tool(
            "kimetsu_benchmark_record_outcome",
            json!({
                "task": "compile-compcert",
                "warm_policy": "full_warm",
                "mode": "required-kimetsu",
                "passed": true,
                "summary": "Configure expected tools first and keep make logs compact.",
                "commands": ["./configure", "make -j2 >/tmp/build.log 2>&1"],
                "verify": ["run-tests"]
            }),
            &root,
            &SkillConfig::default(),
        )
        .expect("record outcome");

        assert_eq!(result["ok"].as_bool(), Some(true));
        assert_eq!(result["task_slug"].as_str(), Some("compile-compcert"));
        let memories = project::list_memories(&root).expect("list memories");
        assert!(
            memories
                .iter()
                .any(|memory| memory.text.contains("[terminal-bench:compile-compcert]"))
        );
        fs::remove_dir_all(root).expect("remove temp root");
    }

    #[test]
    fn benchmark_record_outcome_creates_pending_generalized_memory_proposal() {
        let root = temp_root("kimetsu-mcp-benchmark-proposal");
        fs::create_dir_all(&root).expect("create temp root");
        project::init_project(&root, false).expect("init project");

        let result = call_tool(
            "kimetsu_benchmark_record_outcome",
            json!({
                "task": "compile-compcert",
                "warm_policy": "full_warm",
                "mode": "required-kimetsu",
                "passed": true,
                "summary": "The exact build passed after adding a verifier.",
                "generalized_memory": "For generated-artifact tasks with hidden verifiers, build a small checker and validate randomized cases before finalizing.",
                "memory_role": "semantic_operator",
                "task_family": "generated-artifact-verification",
                "applies_to": ["hidden-validator tasks", "generated output tasks"],
                "does_not_apply_to": ["pure package installation tasks"],
                "generalization_rationale": "Reusable workflow, not an exact compile-compcert answer.",
                "generalization_confidence": 0.82
            }),
            &root,
            &SkillConfig::default(),
        )
        .expect("record outcome");

        assert_eq!(result["ok"].as_bool(), Some(true));
        assert!(
            result["text"]
                .as_str()
                .unwrap()
                .contains("memory_role=episodic")
        );
        let proposal_id = result["proposal_id"].as_str().expect("proposal id");
        assert!(
            result["proposal_text"]
                .as_str()
                .unwrap()
                .contains("memory_role=semantic_operator")
        );

        let proposals = project::list_proposals(
            &root,
            project::ProposalFilter {
                status: Some("pending".to_string()),
                ..project::ProposalFilter::default()
            },
        )
        .expect("list proposals");
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].proposal_id, proposal_id);
        assert_eq!(proposals[0].kind, "command");
        assert!(proposals[0].text.contains("Human_review: pending"));

        fs::remove_dir_all(root).expect("remove temp root");
    }

    fn temp_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        // Isolate from any enclosing git repo (e.g. a dev's $HOME repo)
        // so ProjectPaths::discover resolves here, not a shared ancestor.
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    // ── dispatch allowlist tests ──────────────────────────────────────────

    /// (a) dispatch("tools/list", .., None) returns the full catalog.
    #[test]
    fn dispatch_no_allowlist_returns_full_catalog() {
        use std::collections::BTreeSet;
        let result = dispatch(
            "tools/list",
            json!({}),
            Path::new("."),
            &SkillConfig::default(),
            None,
        )
        .expect("dispatch tools/list None");
        let tools = result["tools"].as_array().expect("tools array");
        // Full catalog must contain representative tools from every category.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in &[
            "kimetsu_brain_status",
            "kimetsu_brain_context",
            "kimetsu_brain_record",
            "kimetsu_benchmark_context",
            "kimetsu_benchmark_record_outcome",
            "kimetsu_brain_memory_list",
            "kimetsu_brain_memory_top",
            "kimetsu_brain_memory_add",
            "kimetsu_brain_memory_proposals",
            "kimetsu_brain_memory_accept",
            "kimetsu_brain_memory_reject",
            "kimetsu_brain_memory_invalidate",
            "kimetsu_brain_memory_blame",
            "kimetsu_brain_memory_conflicts",
            "kimetsu_brain_ingest_repo",
            "kimetsu_bridge_status",
            "kimetsu_skills_search",
            "kimetsu_bridge_import",
            "kimetsu_bridge_export",
            "kimetsu_bridge_sync",
            "kimetsu_plugin_install",
            "kimetsu_brain_model_list",
            "kimetsu_brain_model_set",
            "kimetsu_brain_reindex",
            "kimetsu_brain_memory_search",
            "kimetsu_brain_conflict_resolve",
            "kimetsu_brain_prune",
            "kimetsu_brain_config_show",
            "kimetsu_brain_insights",
        ] {
            assert!(
                names.contains(expected),
                "full catalog missing `{expected}`; got: {names:?}"
            );
        }
        // Confirm handle_mcp_method returns the same count (byte-identical path).
        let via_handle = handle_mcp_method(
            "tools/list",
            json!({}),
            Path::new("."),
            &SkillConfig::default(),
        )
        .expect("handle_mcp_method tools/list");
        assert_eq!(
            result["tools"].as_array().unwrap().len(),
            via_handle["tools"].as_array().unwrap().len(),
            "dispatch(None) and handle_mcp_method must return the same number of tools"
        );
        let _ = BTreeSet::<&str>::new(); // suppress unused-import if needed
    }

    /// (b) dispatch("tools/list", .., Some({"kimetsu_brain_record"})) returns ONLY that tool.
    #[test]
    fn dispatch_allowlist_filters_tools_list() {
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert("kimetsu_brain_record");
        let result = dispatch(
            "tools/list",
            json!({}),
            Path::new("."),
            &SkillConfig::default(),
            Some(&set),
        )
        .expect("dispatch filtered tools/list");
        let tools = result["tools"].as_array().expect("tools array");
        assert_eq!(
            tools.len(),
            1,
            "allowlist of 1 tool should yield exactly 1 entry, got: {tools:?}"
        );
        assert_eq!(
            tools[0]["name"].as_str(),
            Some("kimetsu_brain_record"),
            "the returned tool must be kimetsu_brain_record"
        );
    }

    /// (c) dispatch("tools/call", {name:"kimetsu_brain_ingest_repo",..}, Some(set_without_it))
    ///     returns the "not available in remote mode" error without executing.
    #[test]
    fn dispatch_allowlist_blocks_unlisted_tool_call() {
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert("kimetsu_brain_record"); // ingest_repo is NOT in this set
        let err = dispatch(
            "tools/call",
            json!({ "name": "kimetsu_brain_ingest_repo", "arguments": {} }),
            Path::new("."),
            &SkillConfig::default(),
            Some(&set),
        )
        .expect_err("should be blocked by allowlist");
        assert!(
            err.contains("not available in remote mode"),
            "error must mention 'not available in remote mode', got: {err:?}"
        );
        assert!(
            err.contains("kimetsu_brain_ingest_repo"),
            "error must name the blocked tool, got: {err:?}"
        );
    }

    /// v1.0.0: the write gate is a pure decision — env (set = wins, both
    /// directions) > remote deny > local config (default allow). Covering
    /// it here keeps env manipulation out of the dispatch-level tests.
    #[test]
    fn write_tools_decision_precedence() {
        // Env set: truthy enables everywhere (incl. remote), falsy disables
        // everywhere (incl. local config-true).
        assert!(write_tools_decision(Some("1"), true, Some(false)));
        assert!(write_tools_decision(Some("on"), false, Some(false)));
        assert!(!write_tools_decision(Some("0"), false, Some(true)));
        assert!(!write_tools_decision(Some("nope"), false, None));
        // Env unset, remote: always deny — cloned-repo config must not
        // be able to enable writes.
        assert!(!write_tools_decision(None, true, Some(true)));
        assert!(!write_tools_decision(None, true, None));
        // Env unset, local: config decides, default allow.
        assert!(write_tools_decision(None, false, Some(true)));
        assert!(!write_tools_decision(None, false, Some(false)));
        assert!(write_tools_decision(None, false, None));
    }

    /// Local dispatch honors `kimetsu.mcp_write_tools = false`: the user's
    /// personalization knob still hard-blocks privileged writes.
    #[test]
    fn dispatch_blocks_writes_when_config_disables_them() {
        let root = temp_root("dispatch-write-gate-config");
        fs::create_dir_all(&root).expect("create temp root");
        kimetsu_brain::project::init_project(&root, false).expect("init project");
        // Flip the knob the way `kimetsu config set` would.
        let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
        let mut config = kimetsu_brain::project::load_config(&paths).expect("load config");
        config.kimetsu.mcp_write_tools = false;
        fs::write(&paths.project_toml, config.to_toml().expect("toml")).expect("write config");

        let err = dispatch(
            "tools/call",
            json!({
                "name": "kimetsu_brain_record",
                "arguments": { "lesson": "persist this without approval" }
            }),
            &root,
            &SkillConfig::default(),
            None,
        )
        .expect_err("config-disabled write tools must be blocked");
        assert!(err.contains("requires explicit approval"));

        fs::remove_dir_all(&root).ok();
    }

    /// Remote dispatch (allowed_tools = Some) ignores workspace config —
    /// even `mcp_write_tools = true` in a (cloned, untrusted) project.toml
    /// must not enable writes without the operator's env var.
    #[test]
    fn dispatch_remote_ignores_config_for_write_tools() {
        use std::collections::BTreeSet;
        let root = temp_root("dispatch-write-gate-remote");
        fs::create_dir_all(&root).expect("create temp root");
        kimetsu_brain::project::init_project(&root, false).expect("init project");
        // Default config already has mcp_write_tools = true.

        let mut allowed = BTreeSet::new();
        allowed.insert("kimetsu_brain_record");
        let err = dispatch(
            "tools/call",
            json!({
                "name": "kimetsu_brain_record",
                "arguments": { "lesson": "persist this without approval" }
            }),
            &root,
            &SkillConfig::default(),
            Some(&allowed),
        )
        .expect_err("remote write tools must stay env-gated");
        assert!(err.contains("requires explicit approval"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn global_plugin_install_is_not_available_through_mcp_helper() {
        let err = call_tool(
            "kimetsu_plugin_install",
            json!({ "target": "codex", "scope": "global" }),
            Path::new("."),
            &SkillConfig::default(),
        )
        .expect_err("global plugin install must be blocked");
        assert!(err.contains("global plugin install is not available"));
    }

    /// (d) An allowed tools/call dispatches correctly (uses kimetsu_brain_status
    ///     which returns initialized:false for a missing brain without executing any
    ///     side-effects, so it's safe in a unit test).
    #[test]
    fn dispatch_allowlist_permits_listed_tool_call() {
        use std::collections::BTreeSet;
        let root = temp_root("dispatch-allowlist-permitted");
        fs::create_dir_all(&root).expect("create temp root");
        let mut set = BTreeSet::new();
        set.insert("kimetsu_brain_status");
        let result = dispatch(
            "tools/call",
            json!({ "name": "kimetsu_brain_status", "arguments": {} }),
            &root,
            &SkillConfig::default(),
            Some(&set),
        )
        .expect("allowed tool call should not be blocked");
        // Result is wrapped in MCP content envelope.
        let text = result["content"][0]["text"]
            .as_str()
            .expect("content[0].text");
        let inner: Value = serde_json::from_str(text).expect("inner JSON");
        // No brain initialized → initialized:false (not a panic or block error).
        assert_eq!(
            inner["initialized"].as_bool(),
            Some(false),
            "brain not initialized — expected initialized:false"
        );
        fs::remove_dir_all(root).expect("remove temp root");
    }
}
