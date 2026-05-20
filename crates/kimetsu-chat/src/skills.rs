use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_MAX_SKILL_BYTES: usize = 24 * 1024;
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 96 * 1024;
const MAX_DISCOVERED_SKILLS: usize = 512;
const MAX_SKILL_RESOURCES: usize = 160;
const MAX_RENDERED_SKILL_RESOURCES: usize = 40;

#[derive(Debug, Clone)]
pub struct SkillConfig {
    pub enabled: bool,
    pub include_workspace_roots: bool,
    pub roots: Vec<PathBuf>,
    pub selected: Vec<String>,
    pub max_skill_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_workspace_roots: true,
            roots: Vec::new(),
            selected: Vec::new(),
            max_skill_bytes: DEFAULT_MAX_SKILL_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    Codex,
    ClaudeCode,
    Kimetsu,
    Unknown,
}

impl SkillSource {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Kimetsu => "kimetsu",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub root: PathBuf,
    pub path: PathBuf,
    pub resources: Vec<SkillResource>,
}

impl SkillManifest {
    pub fn resource_summary(&self) -> String {
        summarize_resources(&self.resources)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillResourceKind {
    Script,
    Reference,
    Asset,
    Template,
    Resource,
}

impl SkillResourceKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Reference => "reference",
            Self::Asset => "asset",
            Self::Template => "template",
            Self::Resource => "resource",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillResource {
    pub relative_path: String,
    pub kind: SkillResourceKind,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub manifest: SkillManifest,
    pub body: String,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct SkillRegistry {
    workspace: PathBuf,
    skills: Vec<SkillManifest>,
}

impl SkillRegistry {
    pub fn discover(workspace: &Path, config: &SkillConfig) -> Result<Self, String> {
        if !config.enabled {
            return Ok(Self {
                workspace: normalize_path(workspace),
                skills: Vec::new(),
            });
        }

        let mut roots = Vec::new();
        if config.include_workspace_roots {
            roots.extend(default_workspace_roots(workspace));
        }
        roots.extend(config.roots.iter().cloned());

        let mut seen = HashSet::new();
        let mut skills = Vec::new();
        for root in roots {
            discover_root(&root, &mut seen, &mut skills)?;
            if skills.len() >= MAX_DISCOVERED_SKILLS {
                break;
            }
        }
        skills.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.path.cmp(&b.path))
        });

        Ok(Self {
            workspace: normalize_path(workspace),
            skills,
        })
    }

    pub fn skills(&self) -> &[SkillManifest] {
        &self.skills
    }

    pub fn load_selected(
        &self,
        selections: &[String],
        config: &SkillConfig,
    ) -> Result<Vec<LoadedSkill>, String> {
        let mut loaded = Vec::new();
        for selection in selections {
            let skill = self.resolve_or_manifest(selection)?;
            push_loaded_skill(&mut loaded, &skill, config)?;
        }
        Ok(loaded)
    }

    pub fn load_one(&self, selection: &str, config: &SkillConfig) -> Result<LoadedSkill, String> {
        let skill = self.resolve_or_manifest(selection)?;
        load_manifest(&skill, config.max_skill_bytes)
    }

    pub fn load_into(
        &self,
        selection: &str,
        loaded: &mut Vec<LoadedSkill>,
        config: &SkillConfig,
    ) -> Result<bool, String> {
        let skill = self.resolve_or_manifest(selection)?;
        if loaded
            .iter()
            .any(|existing| same_path(&existing.manifest.path, &skill.path))
        {
            return Ok(false);
        }
        push_loaded_skill(loaded, &skill, config)?;
        Ok(true)
    }

    pub fn resolve_or_manifest(&self, selection: &str) -> Result<SkillManifest, String> {
        let selection = selection.trim();
        if selection.is_empty() {
            return Err("empty skill selection".to_string());
        }

        let path = PathBuf::from(selection);
        if path.exists() {
            return manifest_from_path(&path);
        }
        let workspace_path = self.workspace.join(selection);
        if workspace_path.exists() {
            return manifest_from_path(&workspace_path);
        }

        let normalized = selection.to_ascii_lowercase();
        let matches: Vec<&SkillManifest> = self
            .skills
            .iter()
            .filter(|skill| {
                skill.name.eq_ignore_ascii_case(selection)
                    || skill
                        .path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .map(|dir| dir.eq_ignore_ascii_case(selection))
                        .unwrap_or(false)
                    || skill
                        .name
                        .to_ascii_lowercase()
                        .contains(normalized.as_str())
            })
            .collect();

        match matches.as_slice() {
            [skill] => Ok((*skill).clone()),
            [] => Err(format!("skill `{selection}` not found; use `/skills list`")),
            _ => Err(format!(
                "skill `{selection}` is ambiguous: {}",
                matches
                    .iter()
                    .take(8)
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

pub fn render_loaded_skills(skills: &[LoadedSkill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("Loaded agent skills (Agent Skills / Codex / Claude Code compatible folders):\n");
    out.push_str(
        "Treat SKILL.md as the entrypoint, not the entire skill. Bundled scripts, references, assets, templates, and other files are listed below and should be read or executed on demand using Kimetsu workspace tools.\n\n",
    );
    for skill in skills {
        out.push_str(&format!(
            "## Skill: {} [{}]\nroot: {}\nentrypoint: {}\ndescription: {}\n",
            skill.manifest.name,
            skill.manifest.source.as_str(),
            skill.manifest.root.display(),
            skill.manifest.path.display(),
            skill.manifest.description
        ));
        if skill.manifest.resources.is_empty() {
            out.push_str("resources: <none>\n");
        } else {
            out.push_str("resources (load only when needed):\n");
            for resource in skill
                .manifest
                .resources
                .iter()
                .take(MAX_RENDERED_SKILL_RESOURCES)
            {
                let size = resource
                    .bytes
                    .map(|bytes| format!(" {bytes} bytes"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  - [{}] {}{}\n",
                    resource.kind.as_str(),
                    resource.relative_path,
                    size
                ));
            }
            if skill.manifest.resources.len() > MAX_RENDERED_SKILL_RESOURCES {
                out.push_str(&format!(
                    "  - ... {} more resource(s)\n",
                    skill.manifest.resources.len() - MAX_RENDERED_SKILL_RESOURCES
                ));
            }
        }
        if skill.truncated {
            out.push_str("(body truncated to fit Kimetsu's skill context budget)\n");
        }
        out.push_str(skill.body.trim());
        out.push_str("\n\n");
    }
    Some(out)
}

pub fn default_workspace_roots(workspace: &Path) -> Vec<PathBuf> {
    vec![
        workspace.join(".kimetsu").join("skills"),
        workspace.join(".codex").join("skills"),
        workspace.join(".claude").join("skills"),
    ]
}

pub fn loaded_skill_names(skills: &[LoadedSkill]) -> Vec<String> {
    skills
        .iter()
        .map(|skill| skill.manifest.name.clone())
        .collect()
}

fn push_loaded_skill(
    loaded: &mut Vec<LoadedSkill>,
    skill: &SkillManifest,
    config: &SkillConfig,
) -> Result<(), String> {
    if loaded
        .iter()
        .any(|existing| same_path(&existing.manifest.path, &skill.path))
    {
        return Ok(());
    }
    let total: usize = loaded.iter().map(|skill| skill.body.len()).sum();
    if total >= config.max_total_bytes {
        return Err(format!(
            "skill context budget exhausted before loading `{}`",
            skill.name
        ));
    }
    let mut loaded_skill = load_manifest(skill, config.max_skill_bytes)?;
    let remaining = config.max_total_bytes.saturating_sub(total);
    if loaded_skill.body.len() > remaining {
        loaded_skill.body = truncate_chars(&loaded_skill.body, remaining);
        loaded_skill.truncated = true;
    }
    loaded.push(loaded_skill);
    Ok(())
}

fn load_manifest(skill: &SkillManifest, max_skill_bytes: usize) -> Result<LoadedSkill, String> {
    let content = fs::read_to_string(&skill.path)
        .map_err(|err| format!("failed to read skill {}: {err}", skill.path.display()))?;
    let (_, body) = parse_skill_markdown(&content, &skill.path);
    let body = body.trim();
    let (body, truncated) = if body.len() > max_skill_bytes {
        (truncate_chars(body, max_skill_bytes), true)
    } else {
        (body.to_string(), false)
    };
    Ok(LoadedSkill {
        manifest: skill.clone(),
        body,
        truncated,
    })
}

fn discover_root(
    root: &Path,
    seen: &mut HashSet<PathBuf>,
    skills: &mut Vec<SkillManifest>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    let path = normalize_path(root);
    if path.is_file() {
        let manifest = manifest_from_path(&path)?;
        if seen.insert(normalize_path(&manifest.path)) {
            skills.push(manifest);
        }
        return Ok(());
    }
    if path.join("SKILL.md").is_file() {
        let manifest = manifest_from_path(&path)?;
        if seen.insert(normalize_path(&manifest.path)) {
            skills.push(manifest);
        }
        return Ok(());
    }
    discover_dir(&path, seen, skills, 0)
}

fn discover_dir(
    dir: &Path,
    seen: &mut HashSet<PathBuf>,
    skills: &mut Vec<SkillManifest>,
    depth: u8,
) -> Result<(), String> {
    if depth > 8 || skills.len() >= MAX_DISCOVERED_SKILLS {
        return Ok(());
    }

    let entries = fs::read_dir(dir)
        .map_err(|err| format!("failed to scan skill dir {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read skill dir entry: {err}"))?;
        let path = entry.path();
        if path.is_dir() {
            if path.join("SKILL.md").is_file() {
                let manifest = manifest_from_path(&path)?;
                if seen.insert(normalize_path(&manifest.path)) {
                    skills.push(manifest);
                }
            } else {
                discover_dir(&path, seen, skills, depth + 1)?;
            }
        } else if is_markdown_skill_file(&path) {
            let manifest = manifest_from_path(&path)?;
            if seen.insert(normalize_path(&manifest.path)) {
                skills.push(manifest);
            }
        }
        if skills.len() >= MAX_DISCOVERED_SKILLS {
            break;
        }
    }
    Ok(())
}

fn manifest_from_path(path: &Path) -> Result<SkillManifest, String> {
    let path = if path.is_dir() {
        path.join("SKILL.md")
    } else {
        path.to_path_buf()
    };
    if !path.is_file() {
        return Err(format!(
            "{} is not a skill file or directory containing SKILL.md",
            path.display()
        ));
    }
    let content = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read skill {}: {err}", path.display()))?;
    let (meta, body) = parse_skill_markdown(&content, &path);
    let name = meta
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| fallback_skill_name(&path));
    let description = meta
        .description
        .filter(|description| !description.trim().is_empty())
        .unwrap_or_else(|| fallback_description(body));
    Ok(SkillManifest {
        name,
        description,
        source: source_from_path(&path),
        root: normalize_path(path.parent().unwrap_or_else(|| Path::new("."))),
        path: normalize_path(&path),
        resources: discover_skill_resources(path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_default(),
    })
}

fn discover_skill_resources(root: &Path) -> Result<Vec<SkillResource>, String> {
    let mut resources = Vec::new();
    collect_skill_resources(root, root, &mut resources, 0)?;
    resources.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(resources)
}

fn collect_skill_resources(
    root: &Path,
    dir: &Path,
    resources: &mut Vec<SkillResource>,
    depth: u8,
) -> Result<(), String> {
    if depth > 6 || resources.len() >= MAX_SKILL_RESOURCES || !dir.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(dir)
        .map_err(|err| format!("failed to scan skill resources {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read skill resource entry: {err}"))?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name.eq_ignore_ascii_case("SKILL.md") {
            continue;
        }
        if path.is_dir() {
            if should_skip_resource_dir(name) {
                continue;
            }
            collect_skill_resources(root, &path, resources, depth + 1)?;
        } else if path.is_file() {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let relative_path = normalize_skill_resource_path(relative);
            let kind = classify_resource(relative);
            let bytes = entry.metadata().ok().map(|metadata| metadata.len());
            resources.push(SkillResource {
                relative_path,
                kind,
                bytes,
            });
        }
        if resources.len() >= MAX_SKILL_RESOURCES {
            break;
        }
    }
    Ok(())
}

fn should_skip_resource_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | ".hg"
            | ".svn"
            | ".venv"
            | "__pycache__"
            | ".mypy_cache"
            | ".pytest_cache"
            | "node_modules"
            | "target"
            | "venv"
    )
}

fn normalize_skill_resource_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn classify_resource(path: &Path) -> SkillResourceKind {
    let first = path
        .components()
        .find_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .unwrap_or("")
        .to_ascii_lowercase();
    match first.as_str() {
        "scripts" => SkillResourceKind::Script,
        "references" => SkillResourceKind::Reference,
        "assets" => SkillResourceKind::Asset,
        "templates" => SkillResourceKind::Template,
        _ => SkillResourceKind::Resource,
    }
}

fn summarize_resources(resources: &[SkillResource]) -> String {
    if resources.is_empty() {
        return "no resources".to_string();
    }

    let scripts = resources
        .iter()
        .filter(|resource| resource.kind == SkillResourceKind::Script)
        .count();
    let references = resources
        .iter()
        .filter(|resource| resource.kind == SkillResourceKind::Reference)
        .count();
    let assets = resources
        .iter()
        .filter(|resource| resource.kind == SkillResourceKind::Asset)
        .count();
    let templates = resources
        .iter()
        .filter(|resource| resource.kind == SkillResourceKind::Template)
        .count();
    let other = resources
        .iter()
        .filter(|resource| resource.kind == SkillResourceKind::Resource)
        .count();

    let mut parts = Vec::new();
    if scripts > 0 {
        parts.push(format!("{scripts} script{}", plural_suffix(scripts)));
    }
    if references > 0 {
        parts.push(format!(
            "{references} reference{}",
            plural_suffix(references)
        ));
    }
    if assets > 0 {
        parts.push(format!("{assets} asset{}", plural_suffix(assets)));
    }
    if templates > 0 {
        parts.push(format!("{templates} template{}", plural_suffix(templates)));
    }
    if other > 0 {
        parts.push(format!("{other} resource{}", plural_suffix(other)));
    }

    format!(
        "{} resource{}: {}",
        resources.len(),
        plural_suffix(resources.len()),
        parts.join(", ")
    )
}

const fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[derive(Debug, Default)]
struct SkillMeta {
    name: Option<String>,
    description: Option<String>,
}

fn parse_skill_markdown<'a>(content: &'a str, path: &Path) -> (SkillMeta, &'a str) {
    let content = content.trim_start_matches('\u{feff}');
    let Some(rest) = content.strip_prefix("---") else {
        return (SkillMeta::default(), content);
    };
    let rest = rest
        .strip_prefix('\r')
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest);
    let Some(end) = find_frontmatter_end(rest) else {
        return (SkillMeta::default(), content);
    };
    let frontmatter = &rest[..end];
    let body = &rest[end..];
    let body = body
        .strip_prefix("---")
        .and_then(|body| {
            body.strip_prefix('\r')
                .or_else(|| body.strip_prefix('\n'))
                .or(Some(body))
        })
        .unwrap_or(body);
    (parse_frontmatter(frontmatter, path), body)
}

fn find_frontmatter_end(rest: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn parse_frontmatter(frontmatter: &str, path: &Path) -> SkillMeta {
    let mut meta = SkillMeta::default();
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = clean_yaml_scalar(value);
        match key.trim() {
            "name" => meta.name = Some(value),
            "description" => meta.description = Some(value),
            _ => {}
        }
    }
    if meta.name.is_none() {
        meta.name = Some(fallback_skill_name(path));
    }
    meta
}

fn clean_yaml_scalar(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
        .trim()
        .to_string()
}

fn fallback_skill_name(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .or_else(|| path.file_stem())
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string()
}

fn fallback_description(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("No description provided.")
        .chars()
        .take(240)
        .collect()
}

fn source_from_path(path: &Path) -> SkillSource {
    let lowered = path.to_string_lossy().to_ascii_lowercase();
    if lowered.contains(".codex") {
        SkillSource::Codex
    } else if lowered.contains(".claude") {
        SkillSource::ClaudeCode
    } else if lowered.contains(".kimetsu") {
        SkillSource::Kimetsu
    } else {
        SkillSource::Unknown
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(left: &Path, right: &Path) -> bool {
    normalize_path(left) == normalize_path(right)
}

fn is_markdown_skill_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case("SKILL.md"))
        .unwrap_or(false)
}

fn truncate_chars(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut out = String::new();
    for ch in value.chars() {
        if out.len() + ch.len_utf8() > max_bytes.saturating_sub(24) {
            break;
        }
        out.push(ch);
    }
    out.push_str("\n...[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_codex_and_claude_skill_frontmatter() {
        let root = temp_root("skill_parse");
        let codex = root.join(".codex/skills/refactor");
        let claude = root.join(".claude/skills/frontend");
        fs::create_dir_all(codex.join("scripts")).expect("codex scripts dir");
        fs::create_dir_all(codex.join("references")).expect("codex references dir");
        fs::create_dir_all(codex.join("assets")).expect("codex assets dir");
        fs::create_dir_all(codex.join("templates")).expect("codex templates dir");
        fs::create_dir_all(&claude).expect("claude dir");
        fs::write(codex.join("scripts/check.ps1"), "Write-Output ok").expect("write codex script");
        fs::write(codex.join("references/guide.md"), "# Guide").expect("write codex ref");
        fs::write(codex.join("assets/schema.json"), "{}").expect("write codex asset");
        fs::write(codex.join("templates/example.txt"), "template").expect("write codex template");
        fs::write(
            codex.join("SKILL.md"),
            "---\nname: refactor\ndescription: Refactor safely.\n---\n# Refactor\nUse tests.",
        )
        .expect("write codex skill");
        fs::write(
            claude.join("SKILL.md"),
            "---\nname: frontend\ndescription: Build UI.\nlicense: test\n---\n# UI\nDesign well.",
        )
        .expect("write claude skill");

        let registry = SkillRegistry::discover(&root, &SkillConfig::default()).expect("discover");
        assert_eq!(registry.skills().len(), 2);
        let refactor = registry
            .skills()
            .iter()
            .find(|skill| skill.name == "refactor")
            .expect("refactor skill");
        assert_eq!(refactor.description, "Refactor safely.");
        assert_eq!(refactor.source, SkillSource::Codex);
        assert_eq!(refactor.root, normalize_path(&codex));
        assert!(refactor.resources.iter().any(|resource| {
            resource.relative_path == "scripts/check.ps1"
                && resource.kind == SkillResourceKind::Script
        }));
        assert!(refactor.resources.iter().any(|resource| {
            resource.relative_path == "references/guide.md"
                && resource.kind == SkillResourceKind::Reference
        }));
        assert!(refactor.resources.iter().any(|resource| {
            resource.relative_path == "assets/schema.json"
                && resource.kind == SkillResourceKind::Asset
        }));
        assert!(refactor.resources.iter().any(|resource| {
            resource.relative_path == "templates/example.txt"
                && resource.kind == SkillResourceKind::Template
        }));
        assert!(registry.skills().iter().any(|skill| {
            skill.name == "frontend"
                && skill.description == "Build UI."
                && skill.source == SkillSource::ClaudeCode
        }));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn loads_selected_skill_and_renders_context() {
        let root = temp_root("skill_load");
        let dir = root.join(".codex/skills/tester");
        fs::create_dir_all(dir.join("scripts")).expect("dir");
        fs::write(dir.join("scripts/check.ps1"), "Write-Output test").expect("write script");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: tester\ndescription: Test things.\n---\nAlways run focused tests.",
        )
        .expect("write");
        let config = SkillConfig {
            selected: vec![".codex/skills/tester".to_string()],
            ..SkillConfig::default()
        };
        let registry = SkillRegistry::discover(&root, &config).expect("discover");
        let loaded = registry
            .load_selected(&config.selected, &config)
            .expect("load");
        let context = render_loaded_skills(&loaded).expect("context");
        assert!(context.contains("Skill: tester"));
        assert!(context.contains("Treat SKILL.md as the entrypoint"));
        assert!(context.contains("root:"));
        assert!(context.contains("entrypoint:"));
        assert!(context.contains("[script] scripts/check.ps1"));
        assert!(context.contains("Always run focused tests."));
        fs::remove_dir_all(root).ok();
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("kimetsu_{label}_{nanos}"));
        fs::create_dir_all(&root).expect("root");
        root
    }
}
