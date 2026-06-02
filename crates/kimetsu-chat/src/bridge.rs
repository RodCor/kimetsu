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
- Installed Codex hooks use `.codex/hooks.json` and the `UserPromptSubmit` event to run `kimetsu brain context-hook --workspace .`. Optional mode does not block when Kimetsu returns no relevant context.

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
- Installed Codex hooks use `.codex/hooks.json` and the `UserPromptSubmit` event to run `kimetsu brain context-hook --workspace .`; benchmark wrappers should inspect MCP transcripts for required Kimetsu usage.

Kimetsu brain tools retrieve and manage durable context. Kimetsu bridge tools discover and install reusable capabilities. Continue the actual task with the host harness's normal file, shell, edit, and verification tools after loading Kimetsu context.
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

pub fn plugin_install(
    workspace: &Path,
    target: BridgeTarget,
    mode: PluginMode,
    force: bool,
    proactive: bool,
) -> Result<PluginInstallReport, String> {
    let workspace = normalize_path(workspace);
    let mut files = Vec::new();
    match target {
        BridgeTarget::ClaudeCode => {
            // Claude Code reads project-scoped MCP servers from `.mcp.json` at
            // the workspace root, NOT `.claude/mcp.json`.
            let mcp = workspace.join(".mcp.json");
            write_mcp_config(&mcp, force)?;
            files.push(normalize_path(&mcp));

            let commands = workspace.join(".claude").join("commands").join("kimetsu");
            fs::create_dir_all(&commands)
                .map_err(|err| format!("create {}: {err}", commands.display()))?;
            let bridge = commands.join("bridge.md");
            write_text_file(
                &bridge,
                match mode {
                    PluginMode::Optional => CLAUDE_BRIDGE_COMMAND_OPTIONAL,
                    PluginMode::Required => CLAUDE_BRIDGE_COMMAND_REQUIRED,
                },
                force,
            )?;
            files.push(normalize_path(&bridge));
            let delegate = commands.join("delegate.md");
            write_text_file(
                &delegate,
                match mode {
                    PluginMode::Optional => CLAUDE_DELEGATE_COMMAND_OPTIONAL,
                    PluginMode::Required => CLAUDE_DELEGATE_COMMAND_REQUIRED,
                },
                force,
            )?;
            files.push(normalize_path(&delegate));
            // Claude Code hooks live in `.claude/settings.json` under the
            // `hooks` key (keyed by real events, fed via stdin JSON) — not
            // standalone `.ps1`/`.sh` files driven by env vars.
            write_claude_settings(&workspace, force, proactive, &mut files)?;
        }
        BridgeTarget::Codex => {
            let config = workspace.join(".codex").join("config.toml");
            write_codex_config(&config, force)?;
            files.push(normalize_path(&config));

            let skill = workspace
                .join(".codex")
                .join("skills")
                .join("kimetsu-bridge")
                .join("SKILL.md");
            write_text_file(
                &skill,
                match mode {
                    PluginMode::Optional => CODEX_KIMETSU_SKILL_OPTIONAL,
                    PluginMode::Required => CODEX_KIMETSU_SKILL_REQUIRED,
                },
                force,
            )?;
            files.push(normalize_path(&skill));
            write_codex_hooks(&workspace, force, proactive, &mut files)?;
        }
        BridgeTarget::Kimetsu => {
            let dir = workspace.join(".kimetsu").join("extensions");
            fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
            files.push(normalize_path(&dir));
        }
    }
    Ok(PluginInstallReport {
        target,
        mode,
        files,
    })
}

pub fn extensions_root(workspace: &Path) -> PathBuf {
    workspace.join(".kimetsu").join("extensions")
}

fn write_codex_hooks(
    workspace: &Path,
    force: bool,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let hooks = workspace.join(".codex").join("hooks.json");
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
    if hooks_obj.contains_key("UserPromptSubmit") && !force {
        return Err(format!(
            "{} already defines UserPromptSubmit hooks; pass --force",
            hooks.display()
        ));
    }
    hooks_obj.insert(
        "UserPromptSubmit".to_string(),
        serde_json::json!([{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": "kimetsu brain context-hook --workspace .",
                "statusMessage": "Loading Kimetsu brain context",
                "timeout": 30
            }]
        }]),
    );
    if proactive {
        // v0.8: Codex supports PreToolUse/PostToolUse with a regex
        // matcher on tool name (Bash). Both surface a relevant memory
        // via hookSpecificOutput.additionalContext without blocking.
        hooks_obj.insert(
            "PreToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "kimetsu brain pretool-hook --workspace .",
                    "statusMessage": "Kimetsu proactive check",
                    "timeout": 15
                }]
            }]),
        );
        hooks_obj.insert(
            "PostToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "kimetsu brain posttool-hook --workspace .",
                    "statusMessage": "Kimetsu proactive check",
                    "timeout": 15
                }]
            }]),
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

fn write_mcp_config(path: &Path, force: bool) -> Result<(), String> {
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
    let has_kimetsu = root_obj
        .get("servers")
        .and_then(|value| value.as_object())
        .map(|map| map.contains_key("kimetsu"))
        .unwrap_or(false)
        || root_obj
            .get("mcpServers")
            .and_then(|value| value.as_object())
            .map(|map| map.contains_key("kimetsu"))
            .unwrap_or(false);
    if has_kimetsu && !force {
        return Err(format!(
            "{} already has a kimetsu MCP server; pass --force",
            path.display()
        ));
    }
    let server = serde_json::json!({
            "command": "kimetsu",
            "args": ["mcp", "serve", "--workspace", "."]
    });
    insert_mcp_server(root_obj, "servers", server.clone(), path)?;
    insert_mcp_server(root_obj, "mcpServers", server, path)?;
    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize MCP config: {err}"))?;
    write_text_file(path, &text, true)
}

fn write_codex_config(path: &Path, force: bool) -> Result<(), String> {
    let mut root = if path.is_file() {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        toml::from_str::<toml::Value>(&text)
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
    if servers.contains_key("kimetsu") && !force {
        return Err(format!(
            "{} already has a kimetsu MCP server; pass --force",
            path.display()
        ));
    }

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
"#;

/// Write the Claude Code surface that lives under `.claude/`: the brain
/// `CLAUDE.md` guidance and the `settings.json` hook registration.
fn write_claude_settings(
    workspace: &Path,
    force: bool,
    proactive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let claude_dir = workspace.join(".claude");
    fs::create_dir_all(&claude_dir)
        .map_err(|err| format!("create {}: {err}", claude_dir.display()))?;

    // CLAUDE.md: only seed when missing so we never clobber user edits.
    let claude_md = claude_dir.join("CLAUDE.md");
    if !claude_md.is_file() {
        write_text_file(&claude_md, CLAUDE_MD_CONTENT, force)?;
    }
    files.push(normalize_path(&claude_md));

    let settings = claude_dir.join("settings.json");
    write_claude_hooks(&settings, force, proactive)?;
    files.push(normalize_path(&settings));
    Ok(())
}

/// Merge Kimetsu's `UserPromptSubmit`/`Stop` hooks (plus the v0.8
/// proactive `PreToolUse`/`PostToolUse` Bash hooks when `proactive`)
/// into `.claude/settings.json`, preserving any other settings.
fn write_claude_hooks(path: &Path, force: bool, proactive: bool) -> Result<(), String> {
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
    if root_obj.contains_key("hooks") && !force {
        return Err(format!(
            "{} already defines hooks; pass --force",
            path.display()
        ));
    }
    let mut hooks = serde_json::json!({
        "UserPromptSubmit": [{
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain context-hook" }]
        }],
        "Stop": [{
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain stop-hook" }]
        }]
    });
    if proactive && let Some(map) = hooks.as_object_mut() {
        map.insert(
            "PreToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "kimetsu brain pretool-hook" }]
            }]),
        );
        map.insert(
            "PostToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "kimetsu brain posttool-hook" }]
            }]),
        );
    }
    root_obj.insert("hooks".to_string(), hooks);
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

        let required = plugin_install(&root, BridgeTarget::Codex, PluginMode::Required, true, true)
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
            PluginMode::Optional,
            false,
            false,
        )
        .expect("install without proactive");
        let hooks_text = fs::read_to_string(root.join(".codex/hooks.json")).expect("codex hooks");
        let hooks_json: serde_json::Value = serde_json::from_str(&hooks_text).expect("hooks json");
        assert!(hooks_json["hooks"]["UserPromptSubmit"].is_array());
        assert!(
            hooks_json["hooks"]["PreToolUse"].is_null(),
            "proactive disabled must not write PreToolUse"
        );
        assert!(hooks_json["hooks"]["PostToolUse"].is_null());
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
}
