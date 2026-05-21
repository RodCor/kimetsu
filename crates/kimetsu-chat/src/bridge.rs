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
    pub files: Vec<PathBuf>,
}

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
    let skill = registry.resolve_or_manifest(selection)?;
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
    force: bool,
) -> Result<PluginInstallReport, String> {
    let workspace = normalize_path(workspace);
    let mut files = Vec::new();
    match target {
        BridgeTarget::ClaudeCode => {
            let mcp = workspace.join(".claude").join("mcp.json");
            write_mcp_config(&mcp, force)?;
            files.push(normalize_path(&mcp));

            let commands = workspace.join(".claude").join("commands").join("kimetsu");
            fs::create_dir_all(&commands)
                .map_err(|err| format!("create {}: {err}", commands.display()))?;
            let bridge = commands.join("bridge.md");
            write_text_file(
                &bridge,
                "# Kimetsu Bridge\n\nUse the Kimetsu MCP tools to scan, import, export, and sync skills/plugins across Claude Code, Codex, and Kimetsu.\n",
                force,
            )?;
            files.push(normalize_path(&bridge));
            let delegate = commands.join("delegate.md");
            write_text_file(
                &delegate,
                "# Kimetsu Delegate\n\nWhen a task would benefit from Kimetsu's cost-aware harness, call the Kimetsu MCP tools instead of running a full local loop yourself.\n",
                force,
            )?;
            files.push(normalize_path(&delegate));
        }
        BridgeTarget::Codex => {
            let mcp = workspace.join(".codex").join("mcp.json");
            write_mcp_config(&mcp, force)?;
            files.push(normalize_path(&mcp));

            let skill = workspace
                .join(".codex")
                .join("skills")
                .join("kimetsu-bridge")
                .join("SKILL.md");
            write_text_file(
                &skill,
                "---\nname: kimetsu-bridge\ndescription: Use Kimetsu as a cross-harness extension bridge and cost-aware runtime sidecar.\n---\nWhen the user asks to reuse Claude Code/Codex/Kimetsu skills, plugins, memory, review, or task delegation, use the Kimetsu MCP server configured for this workspace.\n",
                force,
            )?;
            files.push(normalize_path(&skill));
        }
        BridgeTarget::Kimetsu => {
            let dir = workspace.join(".kimetsu").join("extensions");
            fs::create_dir_all(&dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
            files.push(normalize_path(&dir));
        }
    }
    Ok(PluginInstallReport { target, files })
}

pub fn extensions_root(workspace: &Path) -> PathBuf {
    workspace.join(".kimetsu").join("extensions")
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
    Ok(registry.resolve_or_manifest(selection)?.root)
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
