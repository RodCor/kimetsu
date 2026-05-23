use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::{benchmark, project};
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use serde_json::{Value, json};

use crate::bridge::{
    BridgeTarget, PluginMode, bridge_export_skill, bridge_import_skill, bridge_scan, bridge_sync,
    plugin_install,
};
use crate::skills::{SkillConfig, SkillRegistry, skill_origin_label};

const KIMETSU_MCP_INSTRUCTIONS: &str = "Kimetsu is a cross-harness sidecar for brain-managed context and portable agent capabilities. Its main value is the Kimetsu brain: retrieved memories, prior outcome signals, repo context, and memory proposal management that can make repeated work cheaper and more consistent. Recommended workflow: call kimetsu_brain_context early on non-trivial coding, review, setup, or broad repository tasks with a concise task query; for Terminal-Bench or other benchmark tasks, call kimetsu_benchmark_context so Kimetsu can detect the task slug, prioritize benchmark memories, and return a compact playbook. Set warm_policy to cold_brain, reactive_warm, or full_warm when reproducing benchmark brain-condition research. Call kimetsu_benchmark_record_outcome after benchmark attempts to write an accepted episodic outcome memory and, when there is a transferable lesson, a pending semantic_operator or anti_pattern memory proposal for human review. Use kimetsu_bridge_status and kimetsu_skills_search when the task may need portable skills from Codex, Claude Code, Agents, or Kimetsu. Bridge tools discover/install capabilities; brain tools retrieve and curate durable context.";

const BRAIN_STATUS_DESCRIPTION: &str = "Inspect the Kimetsu brain for this workspace. Use this to see whether brain.db is initialized, how many memories/runs/proposals exist, and which memories have positive outcome usefulness. Call before relying on memory if you need to know whether the brain has signal.";

const BRAIN_CONTEXT_DESCRIPTION: &str = "Primary Kimetsu brain tool. Call early on non-trivial tasks with a concise task query to retrieve broker-ranked context capsules: accepted memories, repo snippets, manifests, and usefulness-weighted signals. Use the returned capsule summaries as extra working context before planning or editing.";

const BENCHMARK_CONTEXT_DESCRIPTION: &str = "Benchmark-specific Kimetsu brain tool. Use first for Terminal-Bench tasks. It detects or accepts a task slug, retrieves broker-ranked context with benchmark tags, prioritizes accepted semantic_operator and anti_pattern memories over exact episodic run summaries, and returns a compact playbook that Codex/Claude Code should follow before broad exploration. Set warm_policy to cold_brain, reactive_warm, or full_warm to match the benchmark condition being measured.";

const BENCHMARK_RECORD_OUTCOME_DESCRIPTION: &str = "Record a benchmark attempt in Kimetsu brain. This always writes an accepted memory_role=episodic outcome summary for the exact task. Optionally pass generalized_memory with memory_role=semantic_operator or anti_pattern to create a pending human-review memory proposal for a reusable tactic or warning that should transfer beyond one task slug.";

const BRAIN_MEMORY_LIST_DESCRIPTION: &str = "List recent accepted Kimetsu memories with confidence, use count, and usefulness score. Use when you need to understand the durable memory pool or pick a memory id for invalidation.";

const BRAIN_MEMORY_TOP_DESCRIPTION: &str = "List outcome-ranked Kimetsu memories by usefulness_score/use_count. Use this to see which memories have actually helped previous runs and should be trusted more than fresh or low-signal memories.";

const BRAIN_MEMORY_ADD_DESCRIPTION: &str = "Add a durable Kimetsu memory manually. Use only when the user states a reusable preference, convention, command, failure pattern, or fact that should influence future runs. This writes a memory.accepted event.";

const BRAIN_MEMORY_PROPOSALS_DESCRIPTION: &str = "List memory proposals waiting for curation. Use after Kimetsu or a benchmark generated proposed memories so Codex/Claude Code can help review what should become durable brain state.";

const BRAIN_MEMORY_ACCEPT_DESCRIPTION: &str = "Accept a pending Kimetsu memory proposal and promote it into durable memory. Use only when the proposal is reusable beyond the immediate task. This writes a memory.accepted event.";

const BRAIN_MEMORY_REJECT_DESCRIPTION: &str = "Reject a pending Kimetsu memory proposal. Use when the proposal is too task-specific, wrong, duplicated, or unsafe to reuse. This writes a memory.rejected event.";

const BRAIN_MEMORY_INVALIDATE_DESCRIPTION: &str = "Retire an accepted Kimetsu memory so the broker stops retrieving it. Use when a memory is stale, wrong, harmful, or contradicted by newer evidence. This writes a memory.invalidated event.";

const BRAIN_INGEST_REPO_DESCRIPTION: &str = "Index the repository into Kimetsu brain.db so future kimetsu_brain_context calls can retrieve repo snippets and manifests. Use during setup or after major repo changes. This writes repo.ingested events.";

const BRIDGE_STATUS_DESCRIPTION: &str = "Use when deciding whether portable skills/extensions can help the current task. Lists capabilities discovered in this workspace and across user harness roots, plus where each is installed for Kimetsu, Codex, and Claude Code. For Terminal-Bench memory/context, prefer kimetsu_benchmark_context first; for other work, prefer kimetsu_brain_context first.";

const SKILLS_SEARCH_DESCRIPTION: &str = "Search Kimetsu's cross-harness skill catalog for task-specific instructions. Use concise task keywords such as 'terminal-bench mips interpreter', 'github review', or 'phaser game'. Results include root and SKILL.md entrypoint paths; after selecting a relevant result, read its entrypoint with the host harness file tools and follow it.";

const BRIDGE_IMPORT_DESCRIPTION: &str = "Import a discovered skill into this workspace's canonical .kimetsu/extensions registry so Kimetsu can track and re-export it. Use after kimetsu_skills_search or kimetsu_bridge_status identifies a useful external skill that should become portable. This writes files; keep force=false unless replacing an existing import intentionally.";

const BRIDGE_EXPORT_DESCRIPTION: &str = "Export a canonical or discovered skill into a target harness skill root: codex, claude-code, or kimetsu. Use when the current harness needs a skill that Kimetsu found elsewhere, or before running a benchmark where that harness should see the skill natively. This writes files; keep force=false unless replacing intentionally.";

const BRIDGE_SYNC_DESCRIPTION: &str = "Bulk-import all discovered non-Kimetsu skills into .kimetsu/extensions. Use for setup or migration, not during a narrow task unless the user asked to synchronize capabilities. This writes files and may touch many skill bundles.";

const PLUGIN_INSTALL_DESCRIPTION: &str = "Install Kimetsu MCP/plugin wiring for a target harness in this workspace. For codex, writes .codex/mcp.json, the kimetsu-bridge skill, and hook scripts; for claude-code, writes .claude/mcp.json, command docs, and hook scripts. Set mode=optional to recommend brain-first usage and soft-audit hooks, or mode=required to install hooks that block non-trivial work when Kimetsu brain context is unavailable. Installed guidance tells benchmark agents to prefer kimetsu_benchmark_context and record outcomes through kimetsu_benchmark_record_outcome.";

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
            "instructions": KIMETSU_MCP_INSTRUCTIONS,
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

fn call_tool(
    name: &str,
    arguments: Value,
    workspace: &Path,
    skills: &SkillConfig,
) -> Result<Value, String> {
    match name {
        "kimetsu_brain_status" => Ok(kimetsu_brain_status(workspace)),
        "kimetsu_brain_context" => Ok(kimetsu_brain_context(workspace, &arguments)),
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
            let report = plugin_install(workspace, target, mode, force)?;
            Ok(json!({
                "target": report.target.as_str(),
                "mode": report.mode.as_str(),
                "files": report.files,
            }))
        }
        other => Err(format!("unknown Kimetsu MCP tool `{other}`")),
    }
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

fn kimetsu_brain_context(workspace: &Path, arguments: &Value) -> Value {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        return json!({
            "ok": false,
            "error": "missing `query`",
            "usage": "Pass a concise task description, e.g. {\"query\":\"terminal-bench mips interpreter create frame.bmp\",\"stage\":\"implementation\"}."
        });
    }
    let stage = arguments
        .get("stage")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("localization");
    let budget_tokens = u32_arg(arguments, "budget_tokens", 6000, 500, 30000);

    match project::retrieve_context_readonly(workspace, stage, query, budget_tokens) {
        Ok(bundle) => json!({
            "ok": true,
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
            "budget_tokens": bundle.budget_tokens,
            "used_tokens": bundle.used_tokens,
            "capsule_count": bundle.capsules.len(),
            "excluded_count": bundle.excluded.len(),
            "capsules": bundle.capsules,
            "excluded": bundle.excluded,
        }),
        Err(err) => brain_unavailable_json(workspace, &err.to_string()),
    }
}

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
    let stage = arguments
        .get("stage")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("localization");
    let budget_tokens = u32_arg(arguments, "budget_tokens", 2500, 500, 30000);
    let max_capsules = u32_arg(arguments, "max_capsules", 8, 1, 20) as usize;
    let require_benchmark_memory = bool_arg(arguments, "require_benchmark_memory", false);

    match project::retrieve_benchmark_context_readonly(
        workspace,
        task,
        &dataset,
        task_slug.as_deref(),
        warm_policy,
        stage,
        budget_tokens,
        require_benchmark_memory,
        max_capsules,
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
    let limit = u32_arg(arguments, "limit", 50, 1, 100) as usize;
    let memories = project::list_memories(workspace)
        .map_err(|err| format!("kimetsu brain memory list: {err}"))?;
    Ok(json!({
        "memories": memories.iter().take(limit).map(json_memory_row).collect::<Vec<_>>(),
        "usage": "Use kimetsu_brain_memory_top for outcome-ranked trust signals; use memory_id with kimetsu_brain_memory_invalidate to retire stale memories."
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
        &arguments
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("fact")
            .to_string(),
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
                    "budget_tokens": { "type": "integer", "minimum": 500, "maximum": 30000 }
                },
                "required": ["query"]
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
                    "mode": {
                        "type": "string",
                        "enum": ["optional", "required"],
                        "description": "optional recommends Kimetsu brain first; required tells the host harness to block non-trivial work until Kimetsu context is available or explicitly waived. Benchmark guidance prefers kimetsu_benchmark_context."
                    },
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
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
