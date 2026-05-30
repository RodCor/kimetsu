use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub const DEFAULT_MAX_SKILL_BYTES: usize = 24 * 1024;
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 96 * 1024;
const MAX_DISCOVERED_SKILLS: usize = 512;
const MAX_SKILL_RESOURCES: usize = 160;
const MAX_RENDERED_SKILL_RESOURCES: usize = 40;

#[derive(Debug, Clone)]
pub struct SkillConfig {
    pub enabled: bool,
    pub include_workspace_roots: bool,
    pub include_user_roots: bool,
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
            include_user_roots: false,
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
    Agents,
    Kimetsu,
    Unknown,
}

impl SkillSource {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Agents => "agents",
            Self::Kimetsu => "kimetsu",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillRootKind {
    Workspace,
    User,
    Marketplace,
    Extra,
}

impl SkillRootKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::User => "user",
            Self::Marketplace => "marketplace",
            Self::Extra => "extra",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillRoot {
    pub source: SkillSource,
    pub kind: SkillRootKind,
    pub path: PathBuf,
    pub marketplace: Option<String>,
    pub logged_in: bool,
    pub exists: bool,
}

#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub marketplace: Option<String>,
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
    roots: Vec<SkillRoot>,
    skills: Vec<SkillManifest>,
}

impl SkillRegistry {
    pub fn discover(workspace: &Path, config: &SkillConfig) -> Result<Self, String> {
        if !config.enabled {
            return Ok(Self {
                workspace: normalize_path(workspace),
                roots: Vec::new(),
                skills: Vec::new(),
            });
        }

        let roots = discover_skill_roots(workspace, config);
        let mut seen = HashSet::new();
        let mut skills = Vec::new();
        for root in &roots {
            discover_root(root, &mut seen, &mut skills)?;
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
            roots,
            skills,
        })
    }

    pub fn refresh(&mut self, config: &SkillConfig) -> Result<(), String> {
        let next = Self::discover(&self.workspace, config)?;
        self.roots = next.roots;
        self.skills = next.skills;
        Ok(())
    }

    pub fn roots(&self) -> &[SkillRoot] {
        &self.roots
    }

    pub fn skills(&self) -> &[SkillManifest] {
        &self.skills
    }

    pub fn matching_skills(&self, query: &str) -> Vec<&SkillManifest> {
        self.skills
            .iter()
            .filter(|skill| skill_matches_query(skill, query))
            .collect()
    }

    pub fn is_installed(&self, skill: &SkillManifest) -> bool {
        skill.source == SkillSource::Kimetsu
            || self.skills.iter().any(|candidate| {
                candidate.source == SkillSource::Kimetsu
                    && candidate.name.eq_ignore_ascii_case(&skill.name)
            })
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

        if let Some(skill) = preferred_skill_match(selection, &matches) {
            return Ok(skill.clone());
        }

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

    /// Like [`resolve_or_manifest`], but rejects resolutions whose skill root
    /// lives outside the workspace and all configured skill roots.
    ///
    /// Used by the bridge import/export paths, whose `selection` arrives from
    /// untrusted MCP input. The raw-path branches of `resolve_or_manifest`
    /// accept any existing filesystem path, so without this guard a
    /// prompt-injected agent could point `selection` at an arbitrary host
    /// directory that happens to contain a `SKILL.md` (e.g. `~/secrets`) and
    /// have its entire tree — including adjacent secret files — copied into the
    /// workspace, where it becomes agent-readable. Legitimate skills are
    /// discovered under configured roots and resolve by name, so they are
    /// unaffected.
    pub fn resolve_or_manifest_contained(&self, selection: &str) -> Result<SkillManifest, String> {
        let manifest = self.resolve_or_manifest(selection)?;
        if self.root_is_allowed(&manifest.root) {
            Ok(manifest)
        } else {
            Err(format!(
                "skill `{selection}` resolves to `{}`, which is outside the workspace and configured skill roots",
                manifest.root.display()
            ))
        }
    }

    fn root_is_allowed(&self, candidate: &Path) -> bool {
        let candidate = normalize_path(candidate);
        if candidate.starts_with(&self.workspace) {
            return true;
        }
        self.roots
            .iter()
            .filter(|root| root.exists)
            .any(|root| candidate.starts_with(normalize_path(&root.path)))
    }

    pub fn install_as_kimetsu(
        &self,
        selection: &str,
        force: bool,
    ) -> Result<SkillManifest, String> {
        let skill = self.resolve_or_manifest(selection)?;
        install_skill_as_kimetsu(&self.workspace, &skill, force)
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
            "## Skill: {} [{}]\norigin: {}\nroot: {}\nentrypoint: {}\ndescription: {}\n",
            skill.manifest.name,
            skill.manifest.source.as_str(),
            skill_origin_label(&skill.manifest),
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

pub fn discover_skill_roots(workspace: &Path, config: &SkillConfig) -> Vec<SkillRoot> {
    if !config.enabled {
        return Vec::new();
    }

    let mut roots = Vec::new();
    if config.include_workspace_roots {
        for (source, path) in [
            (
                SkillSource::Kimetsu,
                workspace.join(".kimetsu").join("skills"),
            ),
            (SkillSource::Codex, workspace.join(".codex").join("skills")),
            (
                SkillSource::ClaudeCode,
                workspace.join(".claude").join("skills"),
            ),
        ] {
            push_skill_root(
                &mut roots,
                SkillRoot {
                    exists: path.exists(),
                    source,
                    kind: SkillRootKind::Workspace,
                    path,
                    marketplace: None,
                    logged_in: true,
                },
            );
        }
    }
    if config.include_user_roots {
        roots.extend(default_user_skill_roots());
    }
    for root in &config.roots {
        push_skill_root(
            &mut roots,
            SkillRoot {
                exists: root.exists(),
                source: source_from_path(root),
                kind: SkillRootKind::Extra,
                path: root.clone(),
                marketplace: None,
                logged_in: true,
            },
        );
    }
    roots
}

pub fn loaded_skill_names(skills: &[LoadedSkill]) -> Vec<String> {
    skills
        .iter()
        .map(|skill| skill.manifest.name.clone())
        .collect()
}

pub fn skill_origin_label(skill: &SkillManifest) -> String {
    match &skill.marketplace {
        Some(marketplace) if !marketplace.is_empty() => {
            format!("{} marketplace {}", skill.source.as_str(), marketplace)
        }
        _ => skill.source.as_str().to_string(),
    }
}

fn default_user_skill_roots() -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    let Some(home) = home_dir() else {
        return roots;
    };

    let providers = [
        (
            SkillSource::Kimetsu,
            "KIMETSU_HOME",
            ".kimetsu",
            &["KIMETSU_MODEL"][..],
        ),
        (
            SkillSource::Codex,
            "CODEX_HOME",
            ".codex",
            &["OPENAI_API_KEY", "CODEX_HOME"][..],
        ),
        (
            SkillSource::ClaudeCode,
            "CLAUDE_HOME",
            ".claude",
            &[
                "CLAUDE_CODE_OAUTH_TOKEN",
                "ANTHROPIC_API_KEY",
                "CLAUDE_HOME",
            ][..],
        ),
        (
            SkillSource::Agents,
            "AGENTS_HOME",
            ".agents",
            &["AGENTS_HOME"][..],
        ),
    ];

    for (source, env_key, default_dir, login_envs) in providers {
        let mut homes = Vec::new();
        if let Some(path) = env_path(env_key) {
            homes.push(path);
        }
        homes.push(home.join(default_dir));

        for provider_home in dedupe_paths(homes) {
            let logged_in = provider_logged_in(&provider_home, login_envs);
            let skills_root = provider_home.join("skills");
            push_skill_root(
                &mut roots,
                SkillRoot {
                    exists: skills_root.exists(),
                    source: source.clone(),
                    kind: SkillRootKind::User,
                    path: skills_root,
                    marketplace: None,
                    logged_in,
                },
            );
            collect_marketplace_skill_roots(&mut roots, source.clone(), &provider_home, logged_in);
        }
    }

    roots
}

fn collect_marketplace_skill_roots(
    roots: &mut Vec<SkillRoot>,
    source: SkillSource,
    provider_home: &Path,
    logged_in: bool,
) {
    for cache_root in [
        provider_home.join("plugins").join("cache"),
        provider_home.join("marketplaces").join("cache"),
    ] {
        if !cache_root.is_dir() {
            continue;
        }
        collect_marketplace_skill_roots_rec(
            roots,
            source.clone(),
            &cache_root,
            &cache_root,
            0,
            logged_in,
        );
    }
}

fn collect_marketplace_skill_roots_rec(
    roots: &mut Vec<SkillRoot>,
    source: SkillSource,
    cache_root: &Path,
    dir: &Path,
    depth: u8,
    logged_in: bool,
) {
    if depth > 6 || roots.len() >= MAX_DISCOVERED_SKILLS {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_skills_dir = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.eq_ignore_ascii_case("skills"))
            .unwrap_or(false);
        if is_skills_dir {
            push_skill_root(
                roots,
                SkillRoot {
                    exists: true,
                    source: source.clone(),
                    kind: SkillRootKind::Marketplace,
                    marketplace: marketplace_label(cache_root, &path),
                    path,
                    logged_in,
                },
            );
        } else if !should_skip_resource_dir(
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        ) {
            collect_marketplace_skill_roots_rec(
                roots,
                source.clone(),
                cache_root,
                &path,
                depth + 1,
                logged_in,
            );
        }
    }
}

fn marketplace_label(cache_root: &Path, skills_dir: &Path) -> Option<String> {
    let rel = skills_dir.strip_prefix(cache_root).ok()?;
    let parts = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .filter(|part| !part.eq_ignore_ascii_case("skills"))
        .take(2)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn push_skill_root(roots: &mut Vec<SkillRoot>, root: SkillRoot) {
    let normalized = normalize_path(&root.path);
    if roots
        .iter()
        .any(|existing| normalize_path(&existing.path) == normalized)
    {
        return;
    }
    roots.push(SkillRoot {
        path: normalized,
        ..root
    });
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let normalized = normalize_path(&path);
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn provider_logged_in(provider_home: &Path, env_keys: &[&str]) -> bool {
    env_keys
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
        || provider_home.join("auth.json").is_file()
        || provider_home.join("credentials.json").is_file()
        || provider_home.join("config.json").is_file()
        || provider_home.join("config.toml").is_file()
        || provider_home.join("settings.json").is_file()
        || provider_home.exists()
}

fn preferred_skill_match<'a>(
    selection: &str,
    matches: &[&'a SkillManifest],
) -> Option<&'a SkillManifest> {
    let exact = matches
        .iter()
        .copied()
        .filter(|skill| exact_skill_match(skill, selection))
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [skill] => return Some(*skill),
        [] => {}
        _ => {
            let kimetsu = exact
                .iter()
                .copied()
                .filter(|skill| skill.source == SkillSource::Kimetsu)
                .collect::<Vec<_>>();
            if let [skill] = kimetsu.as_slice() {
                return Some(*skill);
            }
        }
    }

    let kimetsu = matches
        .iter()
        .copied()
        .filter(|skill| skill.source == SkillSource::Kimetsu)
        .collect::<Vec<_>>();
    if let [skill] = kimetsu.as_slice() {
        Some(*skill)
    } else {
        None
    }
}

fn skill_matches_query(skill: &SkillManifest, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    skill.name.to_ascii_lowercase().contains(&query)
        || skill.description.to_ascii_lowercase().contains(&query)
        || skill_origin_label(skill)
            .to_ascii_lowercase()
            .contains(&query)
        || skill
            .root
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains(&query)
        || skill
            .resources
            .iter()
            .any(|resource| resource.relative_path.to_ascii_lowercase().contains(&query))
}

fn exact_skill_match(skill: &SkillManifest, selection: &str) -> bool {
    skill.name.eq_ignore_ascii_case(selection)
        || skill
            .path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(|dir| dir.eq_ignore_ascii_case(selection))
            .unwrap_or(false)
}

fn install_skill_as_kimetsu(
    workspace: &Path,
    skill: &SkillManifest,
    force: bool,
) -> Result<SkillManifest, String> {
    let target_root = normalize_path(&workspace.join(".kimetsu").join("skills"));
    let slug = slugify_skill_name(&skill.name);
    let destination = target_root.join(slug);

    if skill.source == SkillSource::Kimetsu && same_path(&skill.root, &destination) {
        return Ok(skill.clone());
    }

    fs::create_dir_all(&target_root)
        .map_err(|err| format!("failed to create {}: {err}", target_root.display()))?;
    if destination.exists() {
        if !force {
            return Err(format!(
                "{} already exists; pass --force or `/skills install --force {}`",
                destination.display(),
                skill.name
            ));
        }
        remove_dir_inside(&destination, &target_root)?;
    }

    copy_skill_tree(&skill.root, &destination)?;
    write_install_origin(skill, &destination)?;
    manifest_from_path_with_origin(&destination, Some(SkillSource::Kimetsu), None)
}

fn remove_dir_inside(path: &Path, allowed_parent: &Path) -> Result<(), String> {
    let parent = allowed_parent
        .canonicalize()
        .map_err(|err| format!("failed to resolve {}: {err}", allowed_parent.display()))?;
    let target = path
        .canonicalize()
        .map_err(|err| format!("failed to resolve {}: {err}", path.display()))?;
    if !target.starts_with(&parent) || target == parent {
        return Err(format!(
            "refusing to remove {} outside {}",
            target.display(),
            parent.display()
        ));
    }
    fs::remove_dir_all(&target)
        .map_err(|err| format!("failed to remove {}: {err}", target.display()))
}

fn copy_skill_tree(source: &Path, destination: &Path) -> Result<(), String> {
    if !source.is_dir() {
        return Err(format!("{} is not a skill directory", source.display()));
    }
    fs::create_dir_all(destination)
        .map_err(|err| format!("failed to create {}: {err}", destination.display()))?;
    for entry in
        fs::read_dir(source).map_err(|err| format!("failed to scan {}: {err}", source.display()))?
    {
        let entry = entry.map_err(|err| format!("failed to read skill entry: {err}"))?;
        let source_path = entry.path();
        let name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if source_path.is_dir() {
            if should_skip_resource_dir(name) {
                continue;
            }
            copy_skill_tree(&source_path, &destination.join(name))?;
        } else if source_path.is_file() {
            fs::copy(&source_path, destination.join(name)).map_err(|err| {
                format!(
                    "failed to copy {} into {}: {err}",
                    source_path.display(),
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct SkillInstallOrigin {
    imported_at_unix: u64,
    name: String,
    source: String,
    marketplace: Option<String>,
    original_root: String,
    original_entrypoint: String,
}

fn write_install_origin(skill: &SkillManifest, destination: &Path) -> Result<(), String> {
    let imported_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let origin = SkillInstallOrigin {
        imported_at_unix,
        name: skill.name.clone(),
        source: skill.source.as_str().to_string(),
        marketplace: skill.marketplace.clone(),
        original_root: skill.root.display().to_string(),
        original_entrypoint: skill.path.display().to_string(),
    };
    let json = serde_json::to_string_pretty(&origin)
        .map_err(|err| format!("failed to serialize skill origin: {err}"))?;
    fs::write(destination.join(".kimetsu-skill-origin.json"), json)
        .map_err(|err| format!("failed to write skill origin: {err}"))
}

fn slugify_skill_name(name: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "skill".to_string()
    } else {
        slug.to_string()
    }
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
    root: &SkillRoot,
    seen: &mut HashSet<PathBuf>,
    skills: &mut Vec<SkillManifest>,
) -> Result<(), String> {
    if !root.exists {
        return Ok(());
    }
    let path = normalize_path(&root.path);
    if path.is_file() {
        let manifest = manifest_from_path_with_origin(
            &path,
            Some(root.source.clone()),
            root.marketplace.clone(),
        )?;
        if seen.insert(normalize_path(&manifest.path)) {
            skills.push(manifest);
        }
        return Ok(());
    }
    if path.join("SKILL.md").is_file() {
        let manifest = manifest_from_path_with_origin(
            &path,
            Some(root.source.clone()),
            root.marketplace.clone(),
        )?;
        if seen.insert(normalize_path(&manifest.path)) {
            skills.push(manifest);
        }
        return Ok(());
    }
    discover_dir(&path, root, seen, skills, 0)
}

fn discover_dir(
    dir: &Path,
    root: &SkillRoot,
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
                let manifest = manifest_from_path_with_origin(
                    &path,
                    Some(root.source.clone()),
                    root.marketplace.clone(),
                )?;
                if seen.insert(normalize_path(&manifest.path)) {
                    skills.push(manifest);
                }
            } else {
                discover_dir(&path, root, seen, skills, depth + 1)?;
            }
        } else if is_markdown_skill_file(&path) {
            let manifest = manifest_from_path_with_origin(
                &path,
                Some(root.source.clone()),
                root.marketplace.clone(),
            )?;
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
    manifest_from_path_with_origin(path, None, None)
}

fn manifest_from_path_with_origin(
    path: &Path,
    source: Option<SkillSource>,
    marketplace: Option<String>,
) -> Result<SkillManifest, String> {
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
        source: source.unwrap_or_else(|| source_from_path(&path)),
        marketplace,
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
        if name.eq_ignore_ascii_case("SKILL.md")
            || name.eq_ignore_ascii_case(".kimetsu-skill-origin.json")
        {
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
    } else if lowered.contains(".agents") {
        SkillSource::Agents
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

    #[test]
    fn installs_external_skill_as_kimetsu_bundle() {
        let root = temp_root("skill_install");
        let dir = root.join(".codex/skills/reviewer");
        fs::create_dir_all(dir.join("references")).expect("dir");
        fs::write(dir.join("references/checklist.md"), "# Checklist").expect("write ref");
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: Review diffs.\n---\nLead with findings.",
        )
        .expect("write skill");

        let config = SkillConfig::default();
        let mut registry = SkillRegistry::discover(&root, &config).expect("discover");
        let installed = registry
            .install_as_kimetsu("reviewer", false)
            .expect("install");
        assert_eq!(installed.source, SkillSource::Kimetsu);
        assert!(installed.root.ends_with(".kimetsu/skills/reviewer"));
        assert!(installed.root.join("SKILL.md").is_file());
        assert!(installed.root.join("references/checklist.md").is_file());
        assert!(installed.root.join(".kimetsu-skill-origin.json").is_file());

        registry.refresh(&config).expect("refresh");
        let resolved = registry
            .resolve_or_manifest("reviewer")
            .expect("resolve installed preference");
        assert_eq!(resolved.source, SkillSource::Kimetsu);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn resolve_contained_rejects_paths_outside_workspace_and_roots() {
        let root = temp_root("skill_contained");
        // A legitimate in-workspace skill (resolvable by name and by path).
        let inside = root.join(".codex/skills/helper");
        fs::create_dir_all(&inside).expect("inside dir");
        fs::write(
            inside.join("SKILL.md"),
            "---\nname: helper\ndescription: In-workspace skill.\n---\n# Helper",
        )
        .expect("write inside skill");

        // An attacker-pointed directory OUTSIDE the workspace that happens to
        // contain a SKILL.md alongside a "secret" file.
        let outside = temp_root("skill_contained_outside");
        fs::write(outside.join("secret.txt"), "sk-ant-api03-SECRET").expect("write secret");
        fs::write(
            outside.join("SKILL.md"),
            "---\nname: evil\ndescription: Outside the workspace.\n---\n# Evil",
        )
        .expect("write outside skill");

        let registry = SkillRegistry::discover(&root, &SkillConfig::default()).expect("discover");

        // In-workspace skill resolves fine by name under the contained path.
        assert!(registry.resolve_or_manifest_contained("helper").is_ok());

        let outside_selection = outside.to_string_lossy().to_string();
        // The un-guarded resolver accepts the arbitrary outside path...
        assert!(registry.resolve_or_manifest(&outside_selection).is_ok());
        // ...but the contained resolver used by the bridge import/export rejects it.
        let err = registry
            .resolve_or_manifest_contained(&outside_selection)
            .expect_err("outside path must be rejected");
        assert!(
            err.contains("outside the workspace"),
            "unexpected error: {err}"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
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
