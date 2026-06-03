use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::skills::{SkillConfig, SkillManifest, SkillRegistry, SkillSource, skill_origin_label};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BridgeTarget {
    ClaudeCode,
    Codex,
    Kimetsu,
}

impl BridgeTarget {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "kimetsu" => Ok(Self::Kimetsu),
            other => Err(format!("unknown bridge target `{other}`")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Kimetsu => "kimetsu",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginMode {
    Optional,
    Required,
}

impl PluginMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "optional" | "default" => Ok(Self::Optional),
            "required" | "force" | "forced" | "strict" => Ok(Self::Required),
            other => Err(format!("unknown plugin mode `{other}`")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }
}

impl Default for PluginMode {
    fn default() -> Self {
        Self::Optional
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeExtensionManifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub source: String,
    pub origin: String,
    pub imported_at_unix: u64,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BridgeExtension {
    pub manifest: BridgeExtensionManifest,
    pub root: PathBuf,
    pub skill_entrypoint: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BridgeSkillStatus {
    pub name: String,
    pub description: String,
    pub origin: String,
    pub source_root: PathBuf,
    pub kimetsu_extension: bool,
    pub kimetsu_skill: bool,
    pub claude_skill: bool,
    pub codex_skill: bool,
}

#[derive(Debug, Clone)]
pub struct BridgeScan {
    pub skills: Vec<BridgeSkillStatus>,
    pub extensions: Vec<BridgeExtension>,
}

#[derive(Debug, Clone)]
pub struct PluginInstallReport {
    pub target: BridgeTarget,
    pub scope: InstallScope,
    pub mode: PluginMode,
    pub files: Vec<PathBuf>,
}

const CLAUDE_BRIDGE_COMMAND_OPTIONAL: &str = r#"# Kimetsu Bridge

Use the Kimetsu MCP tools as a brain-first sidecar for this workspace.

Recommended workflow:
1. For Terminal-Bench tasks, call `kimetsu_benchmark_context` first with the task text, dataset, and `warm_policy="reactive_warm"` unless the run explicitly asks for `cold_brain` or `full_warm`; use the returned playbook before broad exploration.
2. Call `kimetsu_brain_context` early on other non-trivial coding, review, debugging, setup, benchmark, or repository tasks. Pass the current task as `query`.
3. Use returned memory, repo, and manifest capsules as working context before planning or editing.
4. After benchmark attempts, call `kimetsu_benchmark_record_outcome` with status, commands, pitfalls, verification, and optional `generalized_memory` (`memory_role=semantic_operator` or `anti_pattern`) so future runs get both exact episodic evidence and reviewed reusable memories.
5. Call `kimetsu_bridge_status` and `kimetsu_skills_search` only when a portable skill or extension may help.
6. Continue the actual task with Claude Code's normal file, shell, edit, and verification tools.

Optional mode: the installed hook attempts to load Kimetsu brain context and records an audit marker, but does not block if Kimetsu is unavailable. For broad work, fix the plugin/MCP setup first.
"#;

const CLAUDE_BRIDGE_COMMAND_REQUIRED: &str = r#"# Kimetsu Bridge

Use the Kimetsu MCP tools as a required brain sidecar for this workspace.

Required workflow:
1. Before planning or editing a Terminal-Bench task, call `kimetsu_benchmark_context` with the task text, dataset, and `warm_policy="full_warm"` unless the run explicitly asks for `cold_brain` or `reactive_warm`; use the returned playbook before broad exploration.
2. Before planning or editing other non-trivial coding, review, debugging, setup, benchmark, or repository tasks, call `kimetsu_brain_context` with a concise `query`.
3. If context is empty or repo capsules are missing, call `kimetsu_brain_status`; call `kimetsu_brain_ingest_repo` when repo indexing is the missing setup step.
4. Use returned memory, repo, and manifest capsules as working context before planning or editing.
5. After benchmark attempts, call `kimetsu_benchmark_record_outcome` with status, commands, pitfalls, verification, and optional `generalized_memory` (`memory_role=semantic_operator` or `anti_pattern`) so future runs get both exact episodic evidence and reviewed reusable memories.
6. Call `kimetsu_bridge_status` and `kimetsu_skills_search` only when a portable skill or extension may help.
7. Continue the actual task with Claude Code's normal file, shell, edit, and verification tools.

Required mode: the installed Claude Code `UserPromptSubmit` hook calls Kimetsu brain context and the `Stop` hook nudges memory recording. Treat missing Kimetsu access as a setup blocker for non-trivial tasks unless the user explicitly waives Kimetsu or the task is trivial.
"#;

const CLAUDE_DELEGATE_COMMAND_OPTIONAL: &str = r#"# Kimetsu Delegate

Use Kimetsu as a sidecar, not as a replacement for normal host tools.

Start with `kimetsu_benchmark_context` for Terminal-Bench tasks (`reactive_warm` in optional mode, `full_warm` in required prefetch mode, `cold_brain` for cold-brain measurements), or `kimetsu_brain_context` for other work that could benefit from durable memory, prior outcomes, or repo-aware retrieval. Then use bridge tools for portable skills or setup work only when needed.
"#;

const CLAUDE_DELEGATE_COMMAND_REQUIRED: &str = r#"# Kimetsu Delegate

Use Kimetsu brain before broad local work.

For Terminal-Bench tasks, first call `kimetsu_benchmark_context` with the run's warm policy (`full_warm` by default in required mode) and use the returned playbook as working context. For other non-trivial coding, review, debugging, setup, benchmark, or repository tasks, first call `kimetsu_brain_context` and use the returned capsules as working context. If the MCP sidecar is unavailable, stop for plugin/MCP setup unless the user explicitly waives Kimetsu or the task is trivial.
"#;

const CODEX_KIMETSU_SKILL_OPTIONAL: &str = r#"---
name: kimetsu-bridge
description: Use Kimetsu brain management and cross-harness extension bridge as a runtime sidecar.
---
Use the Kimetsu MCP server when the task may benefit from Kimetsu brain context, portable skills, workflow memory, review guidance, benchmark setup, or capabilities that may already exist in another harness such as Codex, Claude Code, Agents, or Kimetsu.

Brain-first workflow:
1. For Terminal-Bench tasks, call `kimetsu_benchmark_context` first with the task text, dataset, and `warm_policy="reactive_warm"` unless the run explicitly asks for `cold_brain` or `full_warm`. Use the returned `playbook_markdown` as working memory before broad exploration.
2. Call `kimetsu_brain_context` early on other broad coding, review, debugging, setup, benchmark, or repository tasks. Use a concise query containing the task goal and key technologies.
3. Treat returned memory, repo, and manifest capsules as working context before planning or editing.
4. After benchmark attempts, call `kimetsu_benchmark_record_outcome` with status, commands, pitfalls, verification, and optional `generalized_memory` (`memory_role=semantic_operator` or `anti_pattern`) so future runs retrieve a better playbook without overfitting one task.
5. Call `kimetsu_brain_status` when you need to know whether the brain is initialized, has accepted memories, has pending memory proposals, or has useful outcome-ranked memories.
6. Call `kimetsu_skills_search` with concise task keywords when you need task-specific skill guidance. Read the returned `SKILL.md` entrypoint with normal file tools before following it.
7. Use `kimetsu_bridge_status`, `kimetsu_bridge_import`, `kimetsu_bridge_export`, `kimetsu_bridge_sync`, or `kimetsu_plugin_install` only for setup or when a capability must be made available to another harness.
8. Use `kimetsu_brain_memory_proposals`, `kimetsu_brain_memory_accept`, `kimetsu_brain_memory_reject`, `kimetsu_brain_memory_add`, and `kimetsu_brain_memory_invalidate` only when curating durable memory state.

Optional mode:
- Kimetsu brain is the preferred first step for non-trivial work.
- If native MCP tools are unavailable and the task is small, note that Kimetsu brain context was unavailable and continue.
- For broad work, fix the plugin/MCP setup first so `kimetsu_brain_context` is available.
- Installed Codex hooks use `.codex/hooks.json`: `UserPromptSubmit` runs `kimetsu brain context-hook --workspace .`, `Stop` runs `kimetsu brain stop-hook --workspace . --distill-on-stop`, and proactive hooks wrap Bash tool calls. Optional mode does not block when Kimetsu returns no relevant context.
- The installer also writes `.codex/agents/kimetsu-memory-harvester.toml`. When a `[kimetsu-harvest]` cue appears and no credentialed distiller is configured, spawn that custom agent in the background to record any durable lesson.

Kimetsu brain tools retrieve and manage durable context. Kimetsu bridge tools discover and install reusable capabilities. Continue the actual task with the host harness's normal file, shell, edit, and verification tools.
"#;

const CODEX_KIMETSU_SKILL_REQUIRED: &str = r#"---
name: kimetsu-bridge
description: Require Kimetsu brain management before broad coding, review, setup, or benchmark work.
---
Use the Kimetsu MCP server as a required brain sidecar for this workspace. Kimetsu's main value is brain management: retrieved memories, prior outcome signals, repo context, and memory proposal curation. Bridge tools are secondary setup and portability tools.

Required workflow:
1. Before planning or editing a Terminal-Bench task, call `kimetsu_benchmark_context` with the task text, dataset, and `warm_policy="full_warm"` unless the run explicitly asks for `cold_brain` or `reactive_warm`. Use the returned `playbook_markdown` as working memory before broad exploration.
2. Before planning or editing other non-trivial coding, review, debugging, setup, benchmark, or repository tasks, call `kimetsu_brain_context` with a concise query containing the task goal and key technologies.
3. Treat returned memory, repo, and manifest capsules as working context before deciding the plan or touching files.
4. If `kimetsu_benchmark_context` or `kimetsu_brain_context` returns no useful capsules, call `kimetsu_brain_status`. If repo files are not indexed, call `kimetsu_brain_ingest_repo` when setup is appropriate, then retry the context call.
5. After benchmark attempts, call `kimetsu_benchmark_record_outcome` with status, commands, pitfalls, verification, and optional `generalized_memory` (`memory_role=semantic_operator` or `anti_pattern`) so future runs retrieve a better playbook without overfitting one task.
6. Call `kimetsu_skills_search` only when task-specific skill guidance may help. Read the returned `SKILL.md` entrypoint with normal file tools before following it.
7. Use `kimetsu_bridge_status`, `kimetsu_bridge_import`, `kimetsu_bridge_export`, `kimetsu_bridge_sync`, or `kimetsu_plugin_install` only for setup or when a capability must be made available to another harness.
8. Use `kimetsu_brain_memory_proposals`, `kimetsu_brain_memory_accept`, `kimetsu_brain_memory_reject`, `kimetsu_brain_memory_add`, and `kimetsu_brain_memory_invalidate` only when curating durable memory state.

Required mode:
- Treat missing Kimetsu MCP access as a setup blocker for non-trivial tasks.
- Continue without Kimetsu only when the user explicitly waives it or the task is trivial.
- State whether `kimetsu_benchmark_context` or `kimetsu_brain_context` was called and how many capsules were returned when reporting benchmark or audit results.
- Installed Codex hooks use `.codex/hooks.json`: `UserPromptSubmit` runs `kimetsu brain context-hook --workspace .`, `Stop` runs `kimetsu brain stop-hook --workspace . --distill-on-stop`, and proactive hooks wrap Bash tool calls; benchmark wrappers should inspect MCP transcripts for required Kimetsu usage.
- The installer also writes `.codex/agents/kimetsu-memory-harvester.toml`. When a `[kimetsu-harvest]` cue appears and no credentialed distiller is configured, spawn that custom agent in the background to record any durable lesson.

Kimetsu brain tools retrieve and manage durable context. Kimetsu bridge tools discover and install reusable capabilities. Continue the actual task with the host harness's normal file, shell, edit, and verification tools after loading Kimetsu context.
"#;

/// Codex custom agent installed at `.codex/agents/kimetsu-memory-harvester.toml`.
/// It mirrors the Claude Code harvester agent but uses Codex's standalone TOML
/// custom-agent schema.
const CODEX_MEMORY_HARVESTER_AGENT: &str = r#"name = "kimetsu-memory-harvester"
description = "Distills durable, generalizable lessons from the recent session and records them to the Kimetsu brain. Spawn in the background when a [kimetsu-harvest] hook cue appears, or after solving a non-obvious problem."
model = "gpt-5.3-codex-spark"
model_reasoning_effort = "medium"
sandbox_mode = "read-only"
developer_instructions = """
You are Kimetsu's memory harvester. Given the recent conversation/session context, extract durable lessons worth remembering across future sessions and record them.

What qualifies:
- A non-obvious fix for a command/tool that failed and was then resolved; capture the root cause and fix, generalized beyond one repo path.
- A convention, gotcha, or environment quirk that cost real effort to discover.
- A reusable approach or anti-pattern confirmed by the outcome.

What does not qualify:
- Trivial or well-known facts, one-liners, restatements of docs.
- Anything specific to a single throwaway value with no general lesson.

How to record:
- For each qualifying lesson, at most 3, call kimetsu_brain_record with a concrete actionable lesson, 2-5 domain tags, an optional one-line context, and confidence in [0,1].
- Use kind = "anti_pattern" for things to avoid, "convention" for project norms, otherwise the default.
- If nothing qualifies, do nothing and finish.

Constraints: do not modify files, run shell commands, or take any action other than calling Kimetsu brain MCP tools. Quality over quantity.
"""
"#;

pub fn bridge_scan(workspace: &Path, config: &SkillConfig) -> Result<BridgeScan, String> {
    let workspace = normalize_path(workspace);
    let registry = SkillRegistry::discover(&workspace, config)?;
    let extensions = load_bridge_extensions(&workspace)?;
    let extension_names = extensions
        .iter()
        .map(|extension| extension.manifest.name.to_ascii_lowercase())
        .collect::<std::collections::HashSet<_>>();

    let mut by_name: HashMap<String, BridgeSkillStatus> = HashMap::new();
    for skill in registry.skills() {
        let key = skill.name.to_ascii_lowercase();
        let entry = by_name.entry(key).or_insert_with(|| BridgeSkillStatus {
            name: skill.name.clone(),
            description: skill.description.clone(),
            origin: skill_origin_label(skill),
            source_root: skill.root.clone(),
            kimetsu_extension: false,
            kimetsu_skill: false,
            claude_skill: false,
            codex_skill: false,
        });
        match skill.source {
            SkillSource::Kimetsu => entry.kimetsu_skill = true,
            SkillSource::ClaudeCode => entry.claude_skill = true,
            SkillSource::Codex => entry.codex_skill = true,
            SkillSource::Agents | SkillSource::Unknown => {}
        }
        if skill.source != SkillSource::Kimetsu {
            entry.origin = skill_origin_label(skill);
            entry.source_root = skill.root.clone();
        }
    }
    for name in extension_names {
        if let Some(status) = by_name.get_mut(&name) {
            status.kimetsu_extension = true;
        }
    }
    let mut skills = by_name.into_values().collect::<Vec<_>>();
    skills.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    Ok(BridgeScan { skills, extensions })
}

pub fn load_bridge_extensions(workspace: &Path) -> Result<Vec<BridgeExtension>, String> {
    let root = extensions_root(workspace);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&root).map_err(|err| format!("scan {}: {err}", root.display()))? {
        let entry = entry.map_err(|err| format!("read bridge extension entry: {err}"))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&manifest_path)
            .map_err(|err| format!("read {}: {err}", manifest_path.display()))?;
        let manifest: BridgeExtensionManifest = serde_json::from_str(&text)
            .map_err(|err| format!("parse {}: {err}", manifest_path.display()))?;
        let skill_entrypoint = path.join("SKILL.md");
        out.push(BridgeExtension {
            manifest,
            root: normalize_path(&path),
            skill_entrypoint: skill_entrypoint.is_file().then_some(skill_entrypoint),
        });
    }
    out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    Ok(out)
}

pub fn bridge_import_skill(
    workspace: &Path,
    config: &SkillConfig,
    selection: &str,
    force: bool,
) -> Result<BridgeExtension, String> {
    let workspace = normalize_path(workspace);
    let registry = SkillRegistry::discover(&workspace, config)?;
    let skill = registry.resolve_or_manifest_contained(selection)?;
    import_skill_manifest(&workspace, &skill, force)
}

pub fn bridge_export_skill(
    workspace: &Path,
    config: &SkillConfig,
    selection: &str,
    target: BridgeTarget,
    force: bool,
) -> Result<PathBuf, String> {
    let workspace = normalize_path(workspace);
    let source_root = resolve_bridge_skill_source(&workspace, config, selection)?;
    let name = source_root
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("invalid skill root {}", source_root.display()))?;
    let destination_root = match target {
        BridgeTarget::ClaudeCode => workspace.join(".claude").join("skills").join(&name),
        BridgeTarget::Codex => workspace.join(".codex").join("skills").join(&name),
        BridgeTarget::Kimetsu => workspace.join(".kimetsu").join("skills").join(&name),
    };
    copy_dir_with_replace(&source_root, &destination_root, force)?;
    Ok(normalize_path(&destination_root))
}

pub fn bridge_sync(workspace: &Path, config: &SkillConfig, force: bool) -> Result<usize, String> {
    let workspace = normalize_path(workspace);
    let registry = SkillRegistry::discover(&workspace, config)?;
    let mut imported = 0usize;
    for skill in registry.skills() {
        if skill.source == SkillSource::Kimetsu {
            continue;
        }
        if import_skill_manifest(&workspace, skill, force).is_ok() {
            imported += 1;
        }
    }
    Ok(imported)
}

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
    _force: bool,
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
            // Generated docs are Kimetsu-owned boilerplate (not user-editable),
            // so always overwrite them on install — unlike CLAUDE.md.
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
            // v0.8.5: the memory-harvester subagent the hooks cue the
            // agent to dispatch (a cheap background Haiku distiller).
            let agents = claude_dir.join("agents");
            fs::create_dir_all(&agents)
                .map_err(|err| format!("create {}: {err}", agents.display()))?;
            let harvester = agents.join("kimetsu-memory-harvester.md");
            write_text_file(&harvester, CLAUDE_MEMORY_HARVESTER_AGENT, true)?;
            files.push(normalize_path(&harvester));
            write_claude_settings(&claude_dir, proactive, &mut files)?;
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
            let agents = codex_dir.join("agents");
            fs::create_dir_all(&agents)
                .map_err(|err| format!("create {}: {err}", agents.display()))?;
            let harvester = agents.join("kimetsu-memory-harvester.toml");
            write_text_file(&harvester, CODEX_MEMORY_HARVESTER_AGENT, true)?;
            files.push(normalize_path(&harvester));
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

pub fn extensions_root(workspace: &Path) -> PathBuf {
    workspace.join(".kimetsu").join("extensions")
}

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
    match list
        .iter_mut()
        .find(|existing| is_kimetsu_hook_group(existing))
    {
        Some(slot) => *slot = group,
        None => list.push(group),
    }
}

/// Merge Kimetsu's `UserPromptSubmit`/`Stop` hooks (plus the v0.8 proactive
/// `PreToolUse`/`PostToolUse` Bash hooks when `proactive`) into
/// `.codex/hooks.json`, preserving any other hooks the user has — even on
/// the same events. Idempotent: re-running never duplicates.
///
/// Codex discovers hooks only from the config-layer `hooks.json` file, using
/// real lifecycle events like `UserPromptSubmit`. The proactive
/// `PreToolUse`/`PostToolUse` hooks use a `Bash` matcher so they fire only
/// around shell invocations; they surface a memory check without blocking the
/// tool call.
///
/// Strip a leading UTF-8 BOM so `serde_json`/`toml` (which reject it) can parse
/// config files written by BOM-emitting editors (e.g. older Windows Notepad) —
/// otherwise an existing `settings.json` saved with a BOM fails install with
/// "expected value at line 1 column 1".
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

fn write_codex_hooks(
    codex_dir: &Path,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let hooks = codex_dir.join("hooks.json");
    let mut root = if hooks.is_file() {
        let text =
            fs::read_to_string(&hooks).map_err(|err| format!("read {}: {err}", hooks.display()))?;
        serde_json::from_str::<serde_json::Value>(strip_bom(&text))
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
    upsert_kimetsu_hook(
        hooks_obj,
        "Stop",
        serde_json::json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "kimetsu brain stop-hook --workspace . --distill-on-stop",
                "statusMessage": "Checking Kimetsu memory capture",
                "timeout": 180
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

fn import_skill_manifest(
    workspace: &Path,
    skill: &SkillManifest,
    force: bool,
) -> Result<BridgeExtension, String> {
    let id = slugify(&skill.name);
    let destination = extensions_root(workspace).join(&id);
    copy_dir_with_replace(&skill.root, &destination, force)?;
    let manifest = BridgeExtensionManifest {
        id,
        name: skill.name.clone(),
        description: skill.description.clone(),
        kind: "skill".to_string(),
        source: skill.source.as_str().to_string(),
        origin: skill_origin_label(skill),
        imported_at_unix: now_unix(),
        capabilities: vec!["skill".to_string(), "resources".to_string()],
    };
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("serialize bridge manifest: {err}"))?;
    fs::write(destination.join("manifest.json"), manifest_json)
        .map_err(|err| format!("write bridge manifest: {err}"))?;
    let origin_json = serde_json::json!({
        "source": skill.source.as_str(),
        "origin": skill_origin_label(skill),
        "original_root": skill.root,
        "original_entrypoint": skill.path,
    });
    fs::write(
        destination.join("origin.json"),
        serde_json::to_string_pretty(&origin_json)
            .map_err(|err| format!("serialize bridge origin: {err}"))?,
    )
    .map_err(|err| format!("write bridge origin: {err}"))?;
    Ok(BridgeExtension {
        manifest,
        root: normalize_path(&destination),
        skill_entrypoint: Some(normalize_path(&destination.join("SKILL.md"))),
    })
}

fn resolve_bridge_skill_source(
    workspace: &Path,
    config: &SkillConfig,
    selection: &str,
) -> Result<PathBuf, String> {
    let extensions = load_bridge_extensions(workspace)?;
    let normalized = selection.trim().to_ascii_lowercase();
    if let Some(extension) = extensions.iter().find(|extension| {
        extension.manifest.name.eq_ignore_ascii_case(selection)
            || extension.manifest.id.eq_ignore_ascii_case(selection)
            || extension
                .manifest
                .name
                .to_ascii_lowercase()
                .contains(&normalized)
    }) {
        return Ok(extension.root.clone());
    }
    let registry = SkillRegistry::discover(workspace, config)?;
    Ok(registry.resolve_or_manifest_contained(selection)?.root)
}

/// Upsert the `kimetsu` MCP server into a Claude config file. Idempotent —
/// re-running just rewrites the same entry, preserving all other keys.
/// `only_mcp_servers` is true for `~/.claude.json` (global), which uses
/// only the `mcpServers` key; workspace `.mcp.json` also gets `servers`.
fn write_mcp_config(path: &Path, only_mcp_servers: bool) -> Result<(), String> {
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(strip_bom(&text))
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

fn write_codex_config(path: &Path) -> Result<(), String> {
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        toml::from_str::<toml::Value>(strip_bom(&text))
            .map_err(|err| format!("parse {}: {err}", path.display()))?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let root_table = root
        .as_table_mut()
        .ok_or_else(|| format!("{} must be a TOML table", path.display()))?;
    let servers_value = root_table
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let servers = servers_value
        .as_table_mut()
        .ok_or_else(|| format!("{} `mcp_servers` must be a TOML table", path.display()))?;

    let mut kimetsu = toml::map::Map::new();
    kimetsu.insert(
        "command".to_string(),
        toml::Value::String("kimetsu".to_string()),
    );
    kimetsu.insert(
        "args".to_string(),
        toml::Value::Array(vec![
            toml::Value::String("mcp".to_string()),
            toml::Value::String("serve".to_string()),
            toml::Value::String("--workspace".to_string()),
            toml::Value::String(".".to_string()),
        ]),
    );
    servers.insert("kimetsu".to_string(), toml::Value::Table(kimetsu));

    let text =
        toml::to_string_pretty(&root).map_err(|err| format!("serialize Codex config: {err}"))?;
    write_text_file(path, &text, true)
}

/// `.claude/CLAUDE.md` body teaching the agent to use the brain MCP tools.
const CLAUDE_MD_CONTENT: &str = r#"# Kimetsu brain

You have a persistent memory brain attached via MCP (tools prefixed `mcp__kimetsu__`).

- **Before non-trivial tasks**: call `kimetsu_brain_context` with a short query. If the brain
  has relevant prior knowledge it will return it. If not (`skipped: true`), proceed as normal —
  this is zero overhead.
- **After solving a non-obvious problem**: call `kimetsu_brain_record` with what you learned
  and 2-5 domain tags. Keep lessons concrete and actionable, not platitudes.

Do not call either tool on simple/one-liner tasks. The brain is for things that required real
effort or that you would want to remember next session.

## Auto-harvesting memories

A Kimetsu hook may emit a `[kimetsu-harvest]` cue (after you fix a previously
failing command, or at the end of a non-trivial session that recorded nothing).
When you see one, dispatch the `kimetsu-memory-harvester` subagent **in the
background** (the Task/Agent tool with `run_in_background: true`) so it distills
and records any durable lesson without blocking your work. It runs on a small,
cheap model and records nothing when there's nothing worth saving.
"#;

const CLAUDE_MD_BEGIN: &str = "<!-- kimetsu:begin -->";
const CLAUDE_MD_END: &str = "<!-- kimetsu:end -->";

/// Merge Kimetsu's guidance block into a `CLAUDE.md` without ever clobbering
/// the user's content. The guidance is wrapped in HTML-comment markers so it
/// can be found and updated idempotently:
///   * missing file   -> write the block
///   * markers absent  -> append the block after the user's content
///   * markers present -> replace just the marked region (upgrade in place)
/// Used for both the workspace `.claude/CLAUDE.md` and the global
/// `~/.claude/CLAUDE.md`.
fn merge_claude_md(path: &Path) -> Result<(), String> {
    let block = format!("{CLAUDE_MD_BEGIN}\n{CLAUDE_MD_CONTENT}{CLAUDE_MD_END}\n");
    let raw = if path.is_file() {
        fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?
    } else {
        String::new()
    };
    let existing = strip_bom(&raw);
    let merged = match (existing.find(CLAUDE_MD_BEGIN), existing.find(CLAUDE_MD_END)) {
        (Some(start), Some(end_start)) if end_start >= start => {
            let end = end_start + CLAUDE_MD_END.len();
            let after = existing[end..]
                .strip_prefix('\n')
                .unwrap_or(&existing[end..]);
            format!("{}{block}{after}", &existing[..start])
        }
        (Some(start), _) => {
            // BEGIN present but END missing/malformed: the marked region is corrupt.
            // Replace everything from BEGIN onward with a single fresh block rather
            // than appending a duplicate.
            let before = existing[..start].trim_end_matches('\n');
            if before.is_empty() {
                block
            } else {
                format!("{before}\n\n{block}")
            }
        }
        _ => {
            let mut out = existing.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n'); // blank line separating user content from our block
            }
            out.push_str(&block);
            out
        }
    };
    write_text_file(path, &merged, true)
}

/// v0.8.5: the memory-harvester subagent installed at
/// `.claude/agents/kimetsu-memory-harvester.md`. A cheap, background
/// Haiku distiller the hooks cue the main agent to dispatch — it reads
/// the recent context, distills 0-3 generalizable lessons (favoring
/// hard-won fixes / resolved tool failures), and records each through
/// the confidence-gated `kimetsu_brain_record` MCP tool.
const CLAUDE_MEMORY_HARVESTER_AGENT: &str = r#"---
name: kimetsu-memory-harvester
description: Distills durable, generalizable lessons from the recent session and records them to the Kimetsu brain. Dispatch in the background when a [kimetsu-harvest] hook cue appears, or after solving a non-obvious problem.
model: haiku
tools: mcp__kimetsu__kimetsu_brain_record, mcp__kimetsu__kimetsu_brain_context
---

You are Kimetsu's memory harvester. Given the recent conversation/session context,
extract durable lessons worth remembering across future sessions and record them.

What qualifies (record these):
- A non-obvious fix for a command/tool that failed and was then resolved — capture
  the root cause and the fix, generalized beyond this one repo path.
- A convention, gotcha, or environment quirk that cost real effort to discover.
- A reusable approach or anti-pattern confirmed by the outcome.

What does NOT qualify (record nothing):
- Trivial or well-known facts, one-liners, restatements of docs.
- Anything specific to a single throwaway value with no general lesson.

How to record:
- For each qualifying lesson (at most 3), call `kimetsu_brain_record` with a
  concrete, actionable `lesson`, 2-5 domain `tags`, an optional one-line
  `context`, and a `confidence` in [0,1] (0.8 when you're sure it generalizes,
  lower when unsure — low-confidence lessons become proposals for review).
- Use `kind: "anti_pattern"` for things to avoid, `"convention"` for project
  norms, otherwise the default.
- If nothing qualifies, do nothing and finish. Quality over quantity.

Constraints: do NOT modify files, run commands, or take any action other than
calling the brain tools. Be terse. One brain call per distinct lesson.
"#;

/// Write the Claude Code surface that lives under `.claude/`: the brain
/// `CLAUDE.md` guidance and the `settings.json` hook registration.
fn write_claude_settings(
    claude_dir: &Path,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    fs::create_dir_all(claude_dir)
        .map_err(|err| format!("create {}: {err}", claude_dir.display()))?;

    // CLAUDE.md: merge our guidance into whatever is there (or create it),
    // never overwriting the user's content. See `merge_claude_md`.
    let claude_md = claude_dir.join("CLAUDE.md");
    merge_claude_md(&claude_md)?;
    files.push(normalize_path(&claude_md));

    let settings = claude_dir.join("settings.json");
    write_claude_hooks(&settings, proactive)?;
    files.push(normalize_path(&settings));
    Ok(())
}

/// Merge Kimetsu's `UserPromptSubmit`/`Stop` hooks (plus the v0.8
/// proactive `PreToolUse`/`PostToolUse` Bash hooks when `proactive`)
/// into `settings.json`, preserving any other hooks the user has — even
/// on the same events. Idempotent: re-running never duplicates.
fn write_claude_hooks(path: &Path, proactive: bool) -> Result<(), String> {
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        serde_json::from_str::<serde_json::Value>(strip_bom(&text))
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
    upsert_kimetsu_hook(
        hooks_obj,
        "SessionEnd",
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain session-end-hook" }]
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

fn insert_mcp_server(
    root: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    server: serde_json::Value,
    path: &Path,
) -> Result<(), String> {
    let servers = root
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(map) = servers.as_object_mut() else {
        return Err(format!("{} `{key}` must be a JSON object", path.display()));
    };
    map.insert("kimetsu".to_string(), server);
    Ok(())
}

fn write_text_file(path: &Path, text: &str, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!(
            "{} exists; pass --force to replace",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(path, text).map_err(|err| format!("write {}: {err}", path.display()))
}

fn copy_dir_with_replace(source: &Path, destination: &Path, force: bool) -> Result<(), String> {
    if !source.is_dir() {
        return Err(format!("{} is not a directory", source.display()));
    }
    if destination.exists() {
        if !force {
            return Err(format!(
                "{} exists; pass --force to replace",
                destination.display()
            ));
        }
        remove_dir_checked(destination)?;
    }
    copy_dir(source, destination)
}

fn copy_dir(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|err| format!("create {}: {err}", destination.display()))?;
    for entry in fs::read_dir(source).map_err(|err| format!("scan {}: {err}", source.display()))? {
        let entry = entry.map_err(|err| format!("read dir entry: {err}"))?;
        let source_path = entry.path();
        let name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid path {}", source_path.display()))?;
        if should_skip(name) {
            continue;
        }
        let dest_path = destination.join(name);
        if source_path.is_dir() {
            copy_dir(&source_path, &dest_path)?;
        } else if source_path.is_file() {
            fs::copy(&source_path, &dest_path)
                .map_err(|err| format!("copy {}: {err}", source_path.display()))?;
        }
    }
    Ok(())
}

fn remove_dir_checked(path: &Path) -> Result<(), String> {
    let target = path
        .canonicalize()
        .map_err(|err| format!("resolve {}: {err}", path.display()))?;
    let Some(parent) = target.parent().map(Path::to_path_buf) else {
        return Err(format!("refusing to remove {}", target.display()));
    };
    if !(target.ends_with("extensions")
        || parent.ends_with("extensions")
        || parent.ends_with("skills"))
    {
        return Err(format!("refusing to remove {}", target.display()));
    }
    fs::remove_dir_all(&target).map_err(|err| format!("remove {}: {err}", target.display()))
}

fn should_skip(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git" | ".hg" | ".svn" | "node_modules" | "target" | "__pycache__"
    )
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "extension".to_string()
    } else {
        out.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(
            arr.len(),
            2,
            "re-run is idempotent, no duplicate kimetsu group"
        );
        assert_eq!(arr[0]["hooks"][0]["command"], "my-own-hook");

        // New event with no prior array: creates it.
        let km_stop = json!({ "matcher": "", "hooks": [{ "type": "command", "command": "kimetsu brain stop-hook" }] });
        upsert_kimetsu_hook(&mut hooks, "Stop", km_stop);
        assert_eq!(hooks["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn install_scope_parses_aliases() {
        assert_eq!(InstallScope::parse("").unwrap(), InstallScope::Workspace);
        assert_eq!(
            InstallScope::parse("workspace").unwrap(),
            InstallScope::Workspace
        );
        assert_eq!(
            InstallScope::parse("Local").unwrap(),
            InstallScope::Workspace
        );
        assert_eq!(InstallScope::parse("global").unwrap(), InstallScope::Global);
        assert_eq!(InstallScope::parse("USER").unwrap(), InstallScope::Global);
        assert_eq!(InstallScope::Workspace.as_str(), "workspace");
        assert_eq!(InstallScope::Global.as_str(), "global");
        assert!(InstallScope::parse("nope").is_err());
    }

    #[test]
    fn imports_and_exports_skill_bundle() {
        let root = temp_root("bridge_import_export");
        let skill_dir = root.join(".codex/skills/reviewer");
        fs::create_dir_all(skill_dir.join("references")).expect("dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: Review code.\n---\nLead with findings.",
        )
        .expect("skill");
        fs::write(skill_dir.join("references/checklist.md"), "# Checklist").expect("ref");

        let config = SkillConfig::default();
        let imported = bridge_import_skill(&root, &config, "reviewer", false).expect("import");
        assert!(imported.root.join("SKILL.md").is_file());
        assert!(imported.root.join("manifest.json").is_file());

        let exported =
            bridge_export_skill(&root, &config, "reviewer", BridgeTarget::ClaudeCode, false)
                .expect("export");
        assert!(exported.ends_with(".claude/skills/reviewer"));
        assert!(exported.join("references/checklist.md").is_file());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn plugin_install_writes_optional_and_required_modes() {
        let root = temp_root("plugin_install_modes");

        let optional = plugin_install(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
        )
        .expect("optional install");
        assert_eq!(optional.mode, PluginMode::Optional);
        let skill_path = root.join(".codex/skills/kimetsu-bridge/SKILL.md");
        let optional_text = fs::read_to_string(&skill_path).expect("optional skill");
        assert!(optional_text.contains("Optional mode"));
        assert!(optional_text.contains("kimetsu_brain_context"));
        assert!(optional_text.contains("kimetsu_benchmark_context"));
        assert!(optional_text.contains("kimetsu_benchmark_record_outcome"));
        assert!(!optional_text.contains("kimetsu_harbor"));
        let codex_harvester = root.join(".codex/agents/kimetsu-memory-harvester.toml");
        let harvester_text = fs::read_to_string(&codex_harvester).expect("codex harvester");
        let _: toml::Value = toml::from_str(&harvester_text).expect("harvester toml");
        assert!(harvester_text.contains("name = \"kimetsu-memory-harvester\""));
        assert!(harvester_text.contains("kimetsu_brain_record"));
        let codex_config = root.join(".codex/config.toml");
        let config_text = fs::read_to_string(&codex_config).expect("codex config");
        assert!(config_text.contains("[mcp_servers.kimetsu]"));
        assert!(config_text.contains("command = \"kimetsu\""));

        let hooks_path = root.join(".codex/hooks.json");
        assert!(hooks_path.is_file());
        let hooks_text = fs::read_to_string(&hooks_path).expect("codex hooks");
        let hooks_json: serde_json::Value = serde_json::from_str(&hooks_text).expect("hooks json");
        assert_eq!(
            hooks_json["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str(),
            Some("kimetsu brain context-hook --workspace .")
        );
        assert_eq!(
            hooks_json["hooks"]["Stop"][0]["hooks"][0]["command"].as_str(),
            Some("kimetsu brain stop-hook --workspace . --distill-on-stop")
        );
        // v0.8: proactive on by default wires the Bash PreToolUse/PostToolUse hooks.
        assert_eq!(
            hooks_json["hooks"]["PostToolUse"][0]["matcher"].as_str(),
            Some("Bash")
        );
        assert_eq!(
            hooks_json["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str(),
            Some("kimetsu brain pretool-hook --workspace .")
        );
        assert!(!root.join(".codex/mcp.json").exists());
        assert!(!root.join(".codex/hooks/pre-turn.ps1").exists());

        let required = plugin_install(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Required,
            true,
            true,
        )
        .expect("required install");
        assert_eq!(required.mode, PluginMode::Required);
        let required_text = fs::read_to_string(&skill_path).expect("required skill");
        assert!(required_text.contains("Required mode"));
        assert!(required_text.contains("setup blocker"));
        assert!(required_text.contains("kimetsu_benchmark_context"));
        assert!(required_text.contains("kimetsu_benchmark_record_outcome"));
        assert!(!required_text.contains("kimetsu_harbor"));
        assert!(required.files.iter().any(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == "hooks.json")
                .unwrap_or(false)
        }));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn plugin_install_no_proactive_skips_tool_hooks() {
        let root = temp_root("plugin_install_no_proactive");
        plugin_install(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
        )
        .expect("install without proactive");
        let hooks_text = fs::read_to_string(root.join(".codex/hooks.json")).expect("codex hooks");
        let hooks_json: serde_json::Value = serde_json::from_str(&hooks_text).expect("hooks json");
        assert!(hooks_json["hooks"]["UserPromptSubmit"].is_array());
        assert_eq!(
            hooks_json["hooks"]["Stop"][0]["hooks"][0]["command"].as_str(),
            Some("kimetsu brain stop-hook --workspace . --distill-on-stop")
        );
        assert!(
            hooks_json["hooks"]["PreToolUse"].is_null(),
            "proactive disabled must not write PreToolUse"
        );
        assert!(hooks_json["hooks"]["PostToolUse"].is_null());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn claude_hooks_merge_tolerates_utf8_bom() {
        // A settings.json saved by a BOM-emitting editor (older Notepad)
        // must still parse + merge, not fail with "expected value at line 1".
        let root = temp_root("claude_hooks_bom");
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let body = serde_json::to_string_pretty(&json!({
            "hooks": {
                "UserPromptSubmit": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "user-hook" }] }
                ]
            }
        }))
        .unwrap();
        let settings = claude.join("settings.json");
        fs::write(&settings, format!("\u{feff}{body}")).unwrap(); // leading BOM

        write_claude_hooks(&settings, true).expect("BOM settings.json must merge");

        let value: serde_json::Value =
            serde_json::from_str(strip_bom(&fs::read_to_string(&settings).unwrap()))
                .expect("output parses");
        let ups = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(ups.len(), 2, "user hook kept + kimetsu appended");
        assert_eq!(ups[0]["hooks"][0]["command"], "user-hook");

        fs::remove_dir_all(root).ok();
    }

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
        assert_eq!(
            ups.len(),
            2,
            "user group kept + one kimetsu group, no dupes"
        );
        assert_eq!(ups[0]["hooks"][0]["command"], "user-prompt-thing");
        assert_eq!(ups[1]["hooks"][0]["command"], "kimetsu brain context-hook");
        // Unrelated user event untouched.
        assert_eq!(
            value["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
            "user-subagent-thing"
        );
        // Kimetsu's own events present.
        assert_eq!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "kimetsu brain stop-hook"
        );
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "kimetsu brain pretool-hook"
        );

        fs::remove_dir_all(root).ok();
    }

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
            value["hooks"]["Stop"][0]["hooks"][0]["command"],
            "kimetsu brain stop-hook --workspace . --distill-on-stop"
        );
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "kimetsu brain pretool-hook --workspace ."
        );
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "kimetsu brain posttool-hook --workspace ."
        );

        fs::remove_dir_all(root).ok();
    }

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
        fs::write(
            &claude_json,
            serde_json::to_string(&json!({ "keepme": 1 })).unwrap(),
        )
        .unwrap();
        write_mcp_config(&claude_json, true).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
        assert_eq!(value["keepme"], 1);
        assert_eq!(value["mcpServers"]["kimetsu"]["command"], "kimetsu");
        assert!(
            value.get("servers").is_none(),
            "global writes mcpServers only"
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn plugin_install_refreshes_generated_files_without_force() {
        let root = temp_root("plugin_install_refresh");
        // First install (Codex) writes SKILL.md.
        plugin_install(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
        )
        .unwrap();
        // Second install with force=false must succeed (refresh, not error).
        plugin_install(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Required,
            false,
            true,
        )
        .unwrap();

        let skill = fs::read_to_string(root.join(".codex/skills/kimetsu-bridge/SKILL.md")).unwrap();
        // Prove the file was overwritten with the Required variant, not left as Optional.
        assert!(
            skill.contains("Treat missing Kimetsu MCP access as a setup blocker"),
            "SKILL.md should contain Required-mode wording after second install"
        );

        fs::remove_dir_all(root).ok();
    }

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
        assert!(home.join(".claude/CLAUDE.md").is_file());
        assert!(home.join(".claude/commands/kimetsu/bridge.md").is_file());
        assert!(
            home.join(".claude/agents/kimetsu-memory-harvester.md")
                .is_file()
        );
        assert!(home.join(".claude.json").is_file());
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["kimetsu"]["command"], "kimetsu");
        assert!(value.get("servers").is_none());
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
        assert!(
            home.join(".codex/agents/kimetsu-memory-harvester.toml")
                .is_file()
        );
        assert!(!ws.join(".codex").exists());

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    #[test]
    fn claude_hooks_install_session_end() {
        let root = temp_root("claude_session_end");
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        write_claude_hooks(&settings, true).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(strip_bom(&fs::read_to_string(&settings).unwrap())).unwrap();
        assert_eq!(
            value["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "kimetsu brain session-end-hook"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_fresh_file() {
        let root = temp_root("claude_md_fresh");
        let p = root.join("CLAUDE.md");
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains(CLAUDE_MD_BEGIN));
        assert!(text.contains("# Kimetsu brain"));
        assert!(text.contains(CLAUDE_MD_END));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_preserves_user_content() {
        let root = temp_root("claude_md_preserve");
        let p = root.join("CLAUDE.md");
        fs::write(&p, "# My rules\nAlways use tabs.\n").unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# My rules"));
        assert!(text.contains("Always use tabs."));
        assert!(text.contains("# Kimetsu brain"));
        assert!(
            text.find("My rules").unwrap() < text.find(CLAUDE_MD_BEGIN).unwrap(),
            "user content precedes the kimetsu block"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_idempotent() {
        let root = temp_root("claude_md_idem");
        let p = root.join("CLAUDE.md");
        fs::write(&p, "# Mine\n").unwrap();
        merge_claude_md(&p).unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(
            text.matches(CLAUDE_MD_BEGIN).count(),
            1,
            "no duplicate block"
        );
        assert_eq!(text.matches(CLAUDE_MD_END).count(), 1);
        assert!(text.contains("# Mine"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_upgrades_in_place() {
        let root = temp_root("claude_md_upgrade");
        let p = root.join("CLAUDE.md");
        fs::write(
            &p,
            format!("# Top\n\n{CLAUDE_MD_BEGIN}\nOLD STALE\n{CLAUDE_MD_END}\n\n# Bottom\n"),
        )
        .unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(!text.contains("OLD STALE"), "stale block replaced");
        assert!(text.contains("# Kimetsu brain"));
        assert!(text.contains("# Top"));
        assert!(text.contains("# Bottom"));
        assert_eq!(text.matches(CLAUDE_MD_BEGIN).count(), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_tolerates_bom() {
        let root = temp_root("claude_md_bom");
        let p = root.join("CLAUDE.md");
        fs::write(&p, format!("\u{feff}# My rules\n")).unwrap();
        merge_claude_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# My rules"));
        assert!(text.contains("# Kimetsu brain"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_claude_md_repairs_begin_without_end() {
        let root = temp_root("claude-md-repair");
        let path = root.join("CLAUDE.md");
        // user content + a corrupt half-block: BEGIN but no END
        let corrupt =
            format!("# My rules\n\nKeep it tidy.\n\n{CLAUDE_MD_BEGIN}\nstale half-block\n");
        write_text_file(&path, &corrupt, true).unwrap();

        merge_claude_md(&path).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        // user content preserved
        assert!(out.contains("# My rules"));
        assert!(out.contains("Keep it tidy."));
        // exactly one BEGIN and one END now (the corrupt region was replaced, not duplicated)
        assert_eq!(out.matches(CLAUDE_MD_BEGIN).count(), 1);
        assert_eq!(out.matches(CLAUDE_MD_END).count(), 1);
        // the stale text is gone and our real guidance is present
        assert!(!out.contains("stale half-block"));
        assert!(out.contains("Kimetsu brain"));
        // idempotent: a second merge keeps exactly one block
        merge_claude_md(&path).unwrap();
        let out2 = fs::read_to_string(&path).unwrap();
        assert_eq!(out2.matches(CLAUDE_MD_BEGIN).count(), 1);
        assert_eq!(out2.matches(CLAUDE_MD_END).count(), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn install_preserves_existing_user_claude_md() {
        let root = temp_root("install_claude_md");
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(
            claude.join("CLAUDE.md"),
            "# Personal global instructions\nDo X.\n",
        )
        .unwrap();

        let mut files = Vec::new();
        write_claude_settings(&claude, false, &mut files).unwrap();

        let text = fs::read_to_string(claude.join("CLAUDE.md")).unwrap();
        assert!(
            text.contains("# Personal global instructions"),
            "user content kept"
        );
        assert!(text.contains("Do X."));
        assert!(text.contains("# Kimetsu brain"), "kimetsu block appended");
        assert!(text.contains(CLAUDE_MD_BEGIN));
        fs::remove_dir_all(root).ok();
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kimetsu_{label}_{nanos}"));
        fs::create_dir_all(&path).expect("root");
        path
    }

    // -------------------------------------------------------------------------
    // B1 — Config-merge golden tests
    // -------------------------------------------------------------------------

    /// Golden test: user has a hook on the shared PreToolUse/Bash event (same
    /// matcher Kimetsu uses) AND an unrelated non-shared event. After
    /// write_claude_hooks (run twice), both user groups must survive alongside
    /// exactly one Kimetsu group on each event, with no duplicates.
    #[test]
    fn b1_claude_hooks_golden_shared_pretooluse_event() {
        let root = temp_root("b1_claude_hooks_golden");
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();

        // Seed: user has their own PreToolUse/Bash hook (same event + matcher
        // as Kimetsu's proactive hook) and a user hook on PostToolUse.
        let seed = json!({
            "someOtherSetting": "keep-me",
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "user-pretool-bash-check" }]
                    }
                ],
                "PostToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "user-posttool-bash-check" }]
                    }
                ]
            }
        });
        let settings = claude.join("settings.json");
        fs::write(&settings, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        // First run — proactive=true so Kimetsu also writes PreToolUse/PostToolUse.
        write_claude_hooks(&settings, true).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();

        // (a) user entry on shared PreToolUse survived
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre.iter()
                .any(|g| g["hooks"][0]["command"] == "user-pretool-bash-check"),
            "user PreToolUse/Bash group must survive"
        );
        // (b) Kimetsu's PreToolUse group is present alongside
        assert!(
            pre.iter()
                .any(|g| g["hooks"][0]["command"] == "kimetsu brain pretool-hook"),
            "kimetsu PreToolUse group must be added"
        );
        // (a) user entry on shared PostToolUse survived
        let post = v["hooks"]["PostToolUse"].as_array().unwrap();
        assert!(
            post.iter()
                .any(|g| g["hooks"][0]["command"] == "user-posttool-bash-check"),
            "user PostToolUse/Bash group must survive"
        );
        // (b) Kimetsu's PostToolUse present
        assert!(
            post.iter()
                .any(|g| g["hooks"][0]["command"] == "kimetsu brain posttool-hook"),
            "kimetsu PostToolUse group must be added"
        );
        // unrelated top-level key untouched
        assert_eq!(v["someOtherSetting"], "keep-me");

        // (c) idempotent: second run must not duplicate Kimetsu's groups
        write_claude_hooks(&settings, true).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        let pre2 = v2["hooks"]["PreToolUse"].as_array().unwrap();
        let km_pre_count = pre2
            .iter()
            .filter(|g| g["hooks"][0]["command"] == "kimetsu brain pretool-hook")
            .count();
        assert_eq!(
            km_pre_count, 1,
            "exactly one Kimetsu PreToolUse group after two runs"
        );
        let post2 = v2["hooks"]["PostToolUse"].as_array().unwrap();
        let km_post_count = post2
            .iter()
            .filter(|g| g["hooks"][0]["command"] == "kimetsu brain posttool-hook")
            .count();
        assert_eq!(
            km_post_count, 1,
            "exactly one Kimetsu PostToolUse group after two runs"
        );
        // user groups still there after second run
        assert!(
            pre2.iter()
                .any(|g| g["hooks"][0]["command"] == "user-pretool-bash-check"),
            "user PreToolUse group survives second run"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Golden test: seed a .codex/hooks.json with a user UserPromptSubmit hook
    /// AND a user hook on a non-Kimetsu event. Run write_codex_hooks twice.
    /// Assert: user entries survive, Kimetsu's entries are added alongside,
    /// no duplicate Kimetsu groups.
    #[test]
    fn b1_codex_hooks_golden_with_user_content() {
        let root = temp_root("b1_codex_hooks_golden");
        let codex = root.join(".codex");
        fs::create_dir_all(&codex).unwrap();

        let seed = json!({
            "hooks": {
                "UserPromptSubmit": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-codex-prompt-hook" }]
                    }
                ],
                "SubagentStop": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-codex-subagent-hook" }]
                    }
                ]
            }
        });
        fs::write(
            codex.join("hooks.json"),
            serde_json::to_string_pretty(&seed).unwrap(),
        )
        .unwrap();

        let mut files = Vec::new();
        // First run — proactive=true
        write_codex_hooks(&codex, true, &mut files).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex.join("hooks.json")).unwrap()).unwrap();

        // (a) user UserPromptSubmit hook survived
        let ups = v["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert!(
            ups.iter()
                .any(|g| g["hooks"][0]["command"] == "user-codex-prompt-hook"),
            "user UserPromptSubmit hook must survive"
        );
        // (b) Kimetsu's UserPromptSubmit hook is present
        assert!(
            ups.iter()
                .any(|g| g["hooks"][0]["command"] == "kimetsu brain context-hook --workspace ."),
            "kimetsu UserPromptSubmit hook must be added"
        );
        // (a) non-shared event untouched
        assert_eq!(
            v["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
            "user-codex-subagent-hook"
        );
        // (b) Kimetsu's own events present
        assert!(v["hooks"]["Stop"].is_array());
        assert!(v["hooks"]["PreToolUse"].is_array());
        assert!(v["hooks"]["PostToolUse"].is_array());

        // (c) idempotent: second run
        write_codex_hooks(&codex, true, &mut files).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex.join("hooks.json")).unwrap()).unwrap();
        let ups2 = v2["hooks"]["UserPromptSubmit"].as_array().unwrap();
        let km_count = ups2
            .iter()
            .filter(|g| g["hooks"][0]["command"] == "kimetsu brain context-hook --workspace .")
            .count();
        assert_eq!(
            km_count, 1,
            "exactly one Kimetsu UserPromptSubmit after two runs"
        );
        assert!(
            ups2.iter()
                .any(|g| g["hooks"][0]["command"] == "user-codex-prompt-hook"),
            "user hook survives second run"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Golden test: seed .mcp.json with a user-defined non-Kimetsu MCP server.
    /// Run write_mcp_config (workspace shape, both keys). Assert user server
    /// survives, kimetsu server is added to both keys, idempotent.
    #[test]
    fn b1_mcp_config_golden_preserves_user_server() {
        let root = temp_root("b1_mcp_golden");
        let mcp = root.join(".mcp.json");

        // Seed: user's own MCP server in both keys, plus an unrelated root key.
        let seed = json!({
            "customKey": "do-not-touch",
            "servers": {
                "my-server": { "command": "my-server-cmd", "args": ["--port", "3000"] }
            },
            "mcpServers": {
                "my-server": { "command": "my-server-cmd", "args": ["--port", "3000"] }
            }
        });
        fs::write(&mcp, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        // First run — workspace style (both servers + mcpServers).
        write_mcp_config(&mcp, false).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();

        // (a) user server survives in both keys
        assert_eq!(
            v["servers"]["my-server"]["command"], "my-server-cmd",
            "user servers entry must survive"
        );
        assert_eq!(
            v["mcpServers"]["my-server"]["command"], "my-server-cmd",
            "user mcpServers entry must survive"
        );
        // (b) kimetsu server added in both keys
        assert_eq!(v["servers"]["kimetsu"]["command"], "kimetsu");
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");
        // unrelated root key untouched
        assert_eq!(v["customKey"], "do-not-touch");

        // (c) idempotent: second run leaves user server and kimetsu in place
        write_mcp_config(&mcp, false).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        assert_eq!(v2["servers"]["my-server"]["command"], "my-server-cmd");
        assert_eq!(v2["servers"]["kimetsu"]["command"], "kimetsu");
        assert_eq!(v2["mcpServers"]["my-server"]["command"], "my-server-cmd");
        assert_eq!(v2["mcpServers"]["kimetsu"]["command"], "kimetsu");
        assert_eq!(v2["customKey"], "do-not-touch");
        // Verify no duplicate kimetsu keys (JSON object keys are unique by spec;
        // the map should have exactly two entries in each block).
        assert_eq!(
            v2["servers"].as_object().unwrap().len(),
            2,
            "servers must have exactly my-server + kimetsu, no duplicates"
        );
        assert_eq!(
            v2["mcpServers"].as_object().unwrap().len(),
            2,
            "mcpServers must have exactly my-server + kimetsu, no duplicates"
        );

        fs::remove_dir_all(root).ok();
    }

    // -------------------------------------------------------------------------
    // B2 — Full install-path tests: (ClaudeCode, Codex) × (workspace, global)
    // -------------------------------------------------------------------------

    /// ClaudeCode workspace install: pre-seed CLAUDE.md and settings.json with
    /// user content, run plugin_install_inner, assert all managed files exist
    /// AND user content survives in every merged file.
    #[test]
    fn b2_install_claudecode_workspace_preserves_user_content() {
        let ws = temp_root("b2_cc_ws");
        let home = temp_root("b2_cc_ws_home"); // not used by workspace install

        // Pre-seed .claude/CLAUDE.md with user instructions.
        let claude_dir = ws.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("CLAUDE.md"),
            "# My workspace rules\nAlways write tests first.\n",
        )
        .unwrap();

        // Pre-seed .claude/settings.json with a user hook on a shared event.
        let seed_settings = json!({
            "myTopLevelPref": true,
            "hooks": {
                "UserPromptSubmit": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-ws-hook" }]
                    }
                ]
            }
        });
        fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&seed_settings).unwrap(),
        )
        .unwrap();

        // Pre-seed .mcp.json with a user server.
        let seed_mcp = json!({
            "servers": {
                "my-ws-server": { "command": "my-ws-server-cmd" }
            },
            "mcpServers": {
                "my-ws-server": { "command": "my-ws-server-cmd" }
            }
        });
        fs::write(
            ws.join(".mcp.json"),
            serde_json::to_string_pretty(&seed_mcp).unwrap(),
        )
        .unwrap();

        plugin_install_inner(
            &ws,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
            None, // workspace install — no home injection needed
        )
        .unwrap();

        // All managed files exist.
        assert!(ws.join(".mcp.json").is_file());
        assert!(claude_dir.join("CLAUDE.md").is_file());
        assert!(claude_dir.join("settings.json").is_file());
        assert!(claude_dir.join("commands/kimetsu/bridge.md").is_file());
        assert!(claude_dir.join("commands/kimetsu/delegate.md").is_file());
        assert!(
            claude_dir
                .join("agents/kimetsu-memory-harvester.md")
                .is_file()
        );

        // User CLAUDE.md content survived.
        let md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert!(
            md.contains("# My workspace rules"),
            "user CLAUDE.md content kept"
        );
        assert!(
            md.contains("Always write tests first."),
            "user CLAUDE.md detail kept"
        );
        assert!(md.contains("# Kimetsu brain"), "kimetsu block appended");
        assert!(md.contains(CLAUDE_MD_BEGIN));

        // User settings.json hook survived alongside Kimetsu's.
        let sv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
                .unwrap();
        let ups = sv["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert!(
            ups.iter()
                .any(|g| g["hooks"][0]["command"] == "user-ws-hook"),
            "user hook must survive in settings.json"
        );
        assert!(
            ups.iter()
                .any(|g| g["hooks"][0]["command"] == "kimetsu brain context-hook"),
            "kimetsu hook must be added"
        );
        assert_eq!(sv["myTopLevelPref"], true, "top-level pref must survive");

        // User .mcp.json server survived alongside Kimetsu's.
        let mv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ws.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mv["servers"]["my-ws-server"]["command"], "my-ws-server-cmd");
        assert_eq!(mv["servers"]["kimetsu"]["command"], "kimetsu");
        assert_eq!(
            mv["mcpServers"]["my-ws-server"]["command"],
            "my-ws-server-cmd"
        );
        assert_eq!(mv["mcpServers"]["kimetsu"]["command"], "kimetsu");

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    /// ClaudeCode global install: pre-seed ~/.claude/CLAUDE.md and
    /// ~/.claude/settings.json, run plugin_install_inner with injected home,
    /// assert workspace is untouched and all merged files in home preserve user
    /// content.
    #[test]
    fn b2_install_claudecode_global_preserves_user_content() {
        let ws = temp_root("b2_cc_global_ws");
        let home = temp_root("b2_cc_global_home");

        // Pre-seed ~/.claude/ files.
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("CLAUDE.md"),
            "# Global rules\nUse conventional commits.\n",
        )
        .unwrap();
        let seed_settings = json!({
            "globalPref": 42,
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-global-stop-hook" }]
                    }
                ]
            }
        });
        fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&seed_settings).unwrap(),
        )
        .unwrap();

        // Pre-seed ~/.claude.json with a non-Kimetsu MCP server.
        let seed_claude_json = json!({
            "mcpServers": {
                "user-global-server": { "command": "user-global-cmd" }
            }
        });
        fs::write(
            home.join(".claude.json"),
            serde_json::to_string_pretty(&seed_claude_json).unwrap(),
        )
        .unwrap();

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

        // Workspace must be untouched.
        assert!(
            !ws.join(".claude").exists(),
            "workspace .claude must not be created"
        );
        assert!(
            !ws.join(".mcp.json").exists(),
            "workspace .mcp.json must not be created"
        );

        // All managed files exist in home.
        assert!(claude_dir.join("CLAUDE.md").is_file());
        assert!(claude_dir.join("settings.json").is_file());
        assert!(claude_dir.join("commands/kimetsu/bridge.md").is_file());
        assert!(
            claude_dir
                .join("agents/kimetsu-memory-harvester.md")
                .is_file()
        );
        assert!(home.join(".claude.json").is_file());

        // User CLAUDE.md content survived.
        let md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert!(md.contains("# Global rules"), "user global CLAUDE.md kept");
        assert!(md.contains("Use conventional commits."));
        assert!(md.contains("# Kimetsu brain"));

        // User settings.json hook survived.
        let sv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
                .unwrap();
        let stop_hooks = sv["hooks"]["Stop"].as_array().unwrap();
        assert!(
            stop_hooks
                .iter()
                .any(|g| g["hooks"][0]["command"] == "user-global-stop-hook"),
            "user global Stop hook must survive"
        );
        assert!(
            stop_hooks
                .iter()
                .any(|g| g["hooks"][0]["command"] == "kimetsu brain stop-hook"),
            "kimetsu Stop hook must be added"
        );
        assert_eq!(sv["globalPref"], 42, "top-level pref must survive");

        // User MCP server in ~/.claude.json survived alongside Kimetsu's.
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(
            cj["mcpServers"]["user-global-server"]["command"],
            "user-global-cmd"
        );
        assert_eq!(cj["mcpServers"]["kimetsu"]["command"], "kimetsu");
        // Global installs must not write the `servers` key to ~/.claude.json.
        assert!(
            cj.get("servers").is_none(),
            "global ~/.claude.json must not get a `servers` key"
        );

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    /// Codex workspace install: pre-seed .codex/hooks.json with a user hook,
    /// run plugin_install_inner, assert all managed files exist AND user hook
    /// survives.
    #[test]
    fn b2_install_codex_workspace_preserves_user_content() {
        let ws = temp_root("b2_codex_ws");

        // Pre-seed .codex/hooks.json with a user hook.
        let codex_dir = ws.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let seed_hooks = json!({
            "hooks": {
                "UserPromptSubmit": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-codex-ws-hook" }]
                    }
                ]
            }
        });
        fs::write(
            codex_dir.join("hooks.json"),
            serde_json::to_string_pretty(&seed_hooks).unwrap(),
        )
        .unwrap();

        plugin_install_inner(
            &ws,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
            None,
        )
        .unwrap();

        // All managed files exist.
        assert!(codex_dir.join("config.toml").is_file());
        assert!(codex_dir.join("hooks.json").is_file());
        assert!(codex_dir.join("skills/kimetsu-bridge/SKILL.md").is_file());
        assert!(
            codex_dir
                .join("agents/kimetsu-memory-harvester.toml")
                .is_file()
        );

        // User hook survived alongside Kimetsu's.
        let hv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap())
                .unwrap();
        let ups = hv["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert!(
            ups.iter()
                .any(|g| g["hooks"][0]["command"] == "user-codex-ws-hook"),
            "user Codex workspace hook must survive"
        );
        assert!(
            ups.iter().any(|g| {
                g["hooks"][0]["command"] == "kimetsu brain context-hook --workspace ."
            }),
            "kimetsu Codex UserPromptSubmit must be added"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Codex global install: pre-seed ~/.codex/hooks.json with a user hook,
    /// run plugin_install_inner with injected home, assert workspace is untouched
    /// and user hook survives in the home dir.
    #[test]
    fn b2_install_codex_global_preserves_user_content() {
        let ws = temp_root("b2_codex_global_ws");
        let home = temp_root("b2_codex_global_home");

        // Pre-seed ~/.codex/hooks.json with a user hook on a non-shared event.
        let codex_dir = home.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let seed_hooks = json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-global-codex-stop" }]
                    }
                ]
            }
        });
        fs::write(
            codex_dir.join("hooks.json"),
            serde_json::to_string_pretty(&seed_hooks).unwrap(),
        )
        .unwrap();

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

        // Workspace must be untouched.
        assert!(
            !ws.join(".codex").exists(),
            "workspace .codex must not be created"
        );

        // All managed files exist in home.
        assert!(codex_dir.join("config.toml").is_file());
        assert!(codex_dir.join("hooks.json").is_file());
        assert!(codex_dir.join("skills/kimetsu-bridge/SKILL.md").is_file());
        assert!(
            codex_dir
                .join("agents/kimetsu-memory-harvester.toml")
                .is_file()
        );

        // User Stop hook survived. Kimetsu's Stop hook also present.
        let hv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap())
                .unwrap();
        let stop = hv["hooks"]["Stop"].as_array().unwrap();
        assert!(
            stop.iter()
                .any(|g| g["hooks"][0]["command"] == "user-global-codex-stop"),
            "user global Codex Stop hook must survive"
        );
        assert!(
            stop.iter().any(|g| {
                g["hooks"][0]["command"]
                    == "kimetsu brain stop-hook --workspace . --distill-on-stop"
            }),
            "kimetsu Stop hook must be added"
        );

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    // -------------------------------------------------------------------------
    // B3 — Upgrade idempotency: second install can't corrupt managed files
    // -------------------------------------------------------------------------

    /// Run plugin_install_inner twice over the same workspace (ClaudeCode).
    /// After the first run, snapshot every managed file. After the second run,
    /// assert every file is byte-identical to the snapshot: no duplicate hook
    /// groups, exactly one CLAUDE.md marker block, stable .mcp.json and
    /// settings.json bytes.
    #[test]
    fn b3_upgrade_idempotency_claudecode_workspace() {
        let ws = temp_root("b3_upgrade_cc_ws");

        // Pre-seed user content so the merged files are non-trivial.
        let claude_dir = ws.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("CLAUDE.md"),
            "# User prefs\nDo good work.\n",
        )
        .unwrap();
        let seed_settings = json!({
            "hooks": {
                "UserPromptSubmit": [
                    {
                        "matcher": "",
                        "hooks": [{ "type": "command", "command": "user-upgrade-hook" }]
                    }
                ]
            }
        });
        fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&seed_settings).unwrap(),
        )
        .unwrap();
        let seed_mcp = json!({
            "servers": { "my-upgrade-server": { "command": "my-upgrade-cmd" } },
            "mcpServers": { "my-upgrade-server": { "command": "my-upgrade-cmd" } }
        });
        fs::write(
            ws.join(".mcp.json"),
            serde_json::to_string_pretty(&seed_mcp).unwrap(),
        )
        .unwrap();

        // First install.
        plugin_install_inner(
            &ws,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
            None,
        )
        .unwrap();

        // Snapshot every merged file after the first install.
        let snapshot_claude_md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        let snapshot_settings = fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let snapshot_mcp = fs::read_to_string(ws.join(".mcp.json")).unwrap();

        // Sanity: snapshots contain expected content.
        assert_eq!(snapshot_claude_md.matches(CLAUDE_MD_BEGIN).count(), 1);
        assert!(snapshot_claude_md.contains("# User prefs"));

        // Second install — same args.
        plugin_install_inner(
            &ws,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
            None,
        )
        .unwrap();

        // Every merged file must be byte-identical to the snapshot.
        let after_claude_md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        let after_settings = fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        let after_mcp = fs::read_to_string(ws.join(".mcp.json")).unwrap();

        assert_eq!(
            after_claude_md, snapshot_claude_md,
            "CLAUDE.md must be byte-identical after second install"
        );
        assert_eq!(
            after_settings, snapshot_settings,
            "settings.json must be byte-identical after second install"
        );
        assert_eq!(
            after_mcp, snapshot_mcp,
            ".mcp.json must be byte-identical after second install"
        );

        // Belt-and-suspenders: confirm invariants on the final state.
        assert_eq!(
            after_claude_md.matches(CLAUDE_MD_BEGIN).count(),
            1,
            "exactly one CLAUDE.md begin marker"
        );
        assert_eq!(
            after_claude_md.matches(CLAUDE_MD_END).count(),
            1,
            "exactly one CLAUDE.md end marker"
        );
        let sv: serde_json::Value = serde_json::from_str(&after_settings).unwrap();
        let ups = sv["hooks"]["UserPromptSubmit"].as_array().unwrap();
        let km_ups_count = ups
            .iter()
            .filter(|g| g["hooks"][0]["command"] == "kimetsu brain context-hook")
            .count();
        assert_eq!(
            km_ups_count, 1,
            "exactly one kimetsu UserPromptSubmit hook group"
        );
        assert_eq!(
            ups.iter()
                .filter(|g| g["hooks"][0]["command"] == "user-upgrade-hook")
                .count(),
            1,
            "exactly one user hook group"
        );
        let mv: serde_json::Value = serde_json::from_str(&after_mcp).unwrap();
        assert_eq!(mv["servers"].as_object().unwrap().len(), 2);
        assert_eq!(mv["mcpServers"].as_object().unwrap().len(), 2);

        fs::remove_dir_all(ws).ok();
    }
}
