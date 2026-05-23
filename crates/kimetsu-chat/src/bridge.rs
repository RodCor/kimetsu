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

Required mode: the installed pre-turn hook calls Kimetsu brain context and the post-turn hook audits the marker. Treat missing Kimetsu access as a setup blocker for non-trivial tasks unless the user explicitly waives Kimetsu or the task is trivial.
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
- Installed hooks attempt to load `kimetsu brain context --json` and write audit markers under `.kimetsu/hooks/usage/`, but optional mode does not block on hook failure.

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
- Installed pre-turn and post-turn hooks call `kimetsu brain context --json` and audit markers under `.kimetsu/hooks/usage/`; benchmark wrappers can additionally inspect those markers or MCP transcripts.

Kimetsu brain tools retrieve and manage durable context. Kimetsu bridge tools discover and install reusable capabilities. Continue the actual task with the host harness's normal file, shell, edit, and verification tools after loading Kimetsu context.
"#;

const KIMETSU_HOOK_PS1_TEMPLATE: &str = r#"$ErrorActionPreference = "Stop"

$mode = "__KIMETSU_MODE__"
$event = if ($env:KIMETSU_HOOK_EVENT) { $env:KIMETSU_HOOK_EVENT } else { "pre-turn" }
$workspace = if ($env:KIMETSU_WORKSPACE) { $env:KIMETSU_WORKSPACE } else { (Get-Location).Path }
$inputText = if ($env:KIMETSU_INPUT) { $env:KIMETSU_INPUT } else { "" }
$sessionId = if ($env:KIMETSU_SESSION_ID) { $env:KIMETSU_SESSION_ID } else { "unknown" }
$usageDir = Join-Path $workspace ".kimetsu\hooks\usage"
$usageFile = Join-Path $usageDir "$sessionId.jsonl"

New-Item -ItemType Directory -Force -Path $usageDir | Out-Null

function Get-KimetsuInputHash {
    param([string]$Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
        ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function Test-KimetsuTrivial {
    param([string]$Text)
    $trimmed = $Text.Trim()
    if ($trimmed.Length -eq 0) { return $true }
    $lower = $trimmed.ToLowerInvariant()
    if ($trimmed.Length -le 16 -and $lower -match "^(hi|hello|hey|thanks|thank you|ok|okay|yes|no|status)$") {
        return $true
    }
    return $false
}

$inputHash = Get-KimetsuInputHash $inputText

function Write-KimetsuUsage {
    param(
        [string]$Status,
        [string]$Reason,
        [object]$CapsuleCount
    )
    $record = [ordered]@{
        timestamp = (Get-Date).ToUniversalTime().ToString("o")
        event = $event
        mode = $mode
        status = $Status
        reason = $Reason
        session_id = $sessionId
        input_sha256 = $inputHash
        capsule_count = $CapsuleCount
        tool = "kimetsu brain context"
    }
    ($record | ConvertTo-Json -Compress) | Add-Content -Path $usageFile
}

function Resolve-KimetsuBin {
    if ($env:KIMETSU_BIN) { return $env:KIMETSU_BIN }
    $debugExe = Join-Path $workspace "target\debug\kimetsu.exe"
    if (Test-Path $debugExe) { return $debugExe }
    $releaseExe = Join-Path $workspace "target\release\kimetsu.exe"
    if (Test-Path $releaseExe) { return $releaseExe }
    return "kimetsu"
}

function Complete-KimetsuFailure {
    param([string]$Reason)
    Write-KimetsuUsage "error" $Reason $null
    if ($mode -eq "required") {
        Write-Error $Reason
        exit 1
    }
    Write-Warning $Reason
    exit 0
}

if (Test-KimetsuTrivial $inputText) {
    Write-KimetsuUsage "skipped" "trivial input" $null
    exit 0
}

if ($event -eq "post-turn") {
    $found = $false
    if (Test-Path $usageFile) {
        foreach ($line in Get-Content $usageFile) {
            try {
                $record = $line | ConvertFrom-Json
                if ($record.event -eq "pre-turn" -and $record.status -eq "ok" -and $record.input_sha256 -eq $inputHash) {
                    $found = $true
                    break
                }
            } catch {
                continue
            }
        }
    }
    if ($found) {
        Write-KimetsuUsage "audit-ok" "pre-turn brain context marker found" $null
        exit 0
    }
    $reason = "missing pre-turn kimetsu_brain_context marker for this input"
    Write-KimetsuUsage "audit-missing" $reason $null
    if ($mode -eq "required") {
        Write-Error $reason
        exit 1
    }
    Write-Warning $reason
    exit 0
}

if ($event -ne "pre-turn") {
    Write-KimetsuUsage "ignored" "unsupported hook event" $null
    exit 0
}

$kimetsu = Resolve-KimetsuBin
$query = $inputText.Trim()
$stage = if ($env:KIMETSU_BRAIN_STAGE) { $env:KIMETSU_BRAIN_STAGE } else { "localization" }
$budget = if ($env:KIMETSU_BRAIN_BUDGET_TOKENS) { $env:KIMETSU_BRAIN_BUDGET_TOKENS } else { "4000" }

Push-Location $workspace
try {
    $output = & $kimetsu brain context $query --stage $stage --budget-tokens $budget --json 2>&1
    $exitCode = $LASTEXITCODE
} catch {
    $output = $_.Exception.Message
    $exitCode = 1
} finally {
    Pop-Location
}

if ($exitCode -ne 0) {
    Complete-KimetsuFailure "kimetsu brain context failed: $output"
}

try {
    $payload = ($output | Out-String) | ConvertFrom-Json
    $capsuleCount = [int]$payload.capsule_count
} catch {
    Complete-KimetsuFailure "kimetsu brain context returned invalid JSON"
}

Write-KimetsuUsage "ok" "brain context loaded" $capsuleCount
Write-Output "kimetsu brain context capsules=$capsuleCount"
exit 0
"#;

const KIMETSU_HOOK_SH_TEMPLATE: &str = r#"#!/usr/bin/env bash
set -u

mode="__KIMETSU_MODE__"
event="${KIMETSU_HOOK_EVENT:-pre-turn}"
workspace="${KIMETSU_WORKSPACE:-$PWD}"
input_text="${KIMETSU_INPUT:-}"
session_id="${KIMETSU_SESSION_ID:-unknown}"
usage_dir="$workspace/.kimetsu/hooks/usage"
usage_file="$usage_dir/$session_id.jsonl"
mkdir -p "$usage_dir"

json_escape() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys; print(json.dumps(sys.stdin.read())[1:-1])'
  else
    sed 's/\\/\\\\/g; s/"/\\"/g'
  fi
}

hash_input() {
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s' "$1" | sha256sum | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    printf '%s' "$1" | openssl dgst -sha256 | awk '{print $NF}'
  else
    printf ''
  fi
}

input_hash="$(hash_input "$input_text")"

write_usage() {
  status="$1"
  reason="$2"
  capsule_count="${3:-null}"
  [ -n "$capsule_count" ] || capsule_count="null"
  timestamp="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  esc_event="$(printf '%s' "$event" | json_escape)"
  esc_mode="$(printf '%s' "$mode" | json_escape)"
  esc_status="$(printf '%s' "$status" | json_escape)"
  esc_reason="$(printf '%s' "$reason" | json_escape)"
  esc_session="$(printf '%s' "$session_id" | json_escape)"
  esc_hash="$(printf '%s' "$input_hash" | json_escape)"
  printf '{"timestamp":"%s","event":"%s","mode":"%s","status":"%s","reason":"%s","session_id":"%s","input_sha256":"%s","capsule_count":%s,"tool":"kimetsu brain context"}\n' \
    "$timestamp" "$esc_event" "$esc_mode" "$esc_status" "$esc_reason" "$esc_session" "$esc_hash" "$capsule_count" >> "$usage_file"
}

is_trivial() {
  trimmed="$(printf '%s' "$input_text" | tr -d '\r' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
  [ -n "$trimmed" ] || return 0
  lower="$(printf '%s' "$trimmed" | tr '[:upper:]' '[:lower:]')"
  case "$lower" in
    hi|hello|hey|thanks|"thank you"|ok|okay|yes|no|status)
      [ "${#trimmed}" -le 16 ] && return 0
      ;;
  esac
  return 1
}

resolve_kimetsu_bin() {
  if [ -n "${KIMETSU_BIN:-}" ]; then
    printf '%s' "$KIMETSU_BIN"
  elif [ -x "$workspace/target/debug/kimetsu" ]; then
    printf '%s' "$workspace/target/debug/kimetsu"
  elif [ -x "$workspace/target/release/kimetsu" ]; then
    printf '%s' "$workspace/target/release/kimetsu"
  else
    printf '%s' "kimetsu"
  fi
}

fail_or_warn() {
  reason="$1"
  write_usage "error" "$reason" "null"
  if [ "$mode" = "required" ]; then
    printf '%s\n' "$reason" >&2
    exit 1
  fi
  printf '%s\n' "$reason" >&2
  exit 0
}

if is_trivial; then
  write_usage "skipped" "trivial input" "null"
  exit 0
fi

if [ "$event" = "post-turn" ]; then
  if [ -f "$usage_file" ] && grep -F '"event":"pre-turn"' "$usage_file" | grep -F '"status":"ok"' | grep -F "\"input_sha256\":\"$input_hash\"" >/dev/null 2>&1; then
    write_usage "audit-ok" "pre-turn brain context marker found" "null"
    exit 0
  fi
  reason="missing pre-turn kimetsu_brain_context marker for this input"
  write_usage "audit-missing" "$reason" "null"
  if [ "$mode" = "required" ]; then
    printf '%s\n' "$reason" >&2
    exit 1
  fi
  printf '%s\n' "$reason" >&2
  exit 0
fi

if [ "$event" != "pre-turn" ]; then
  write_usage "ignored" "unsupported hook event" "null"
  exit 0
fi

kimetsu_bin="$(resolve_kimetsu_bin)"
stage="${KIMETSU_BRAIN_STAGE:-localization}"
budget="${KIMETSU_BRAIN_BUDGET_TOKENS:-4000}"
query="$(printf '%s' "$input_text" | tr -d '\r' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"

if output="$(cd "$workspace" && "$kimetsu_bin" brain context "$query" --stage "$stage" --budget-tokens "$budget" --json 2>&1)"; then
  capsule_count="$(printf '%s\n' "$output" | sed -n 's/.*"capsule_count":[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1)"
  if [ -z "$capsule_count" ]; then
    fail_or_warn "kimetsu brain context returned invalid JSON"
  fi
  write_usage "ok" "brain context loaded" "$capsule_count"
  printf 'kimetsu brain context capsules=%s\n' "$capsule_count"
  exit 0
fi

fail_or_warn "kimetsu brain context failed: $output"
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
    mode: PluginMode,
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
            write_plugin_hooks(
                &workspace,
                BridgeTarget::ClaudeCode,
                mode,
                force,
                &mut files,
            )?;
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
                match mode {
                    PluginMode::Optional => CODEX_KIMETSU_SKILL_OPTIONAL,
                    PluginMode::Required => CODEX_KIMETSU_SKILL_REQUIRED,
                },
                force,
            )?;
            files.push(normalize_path(&skill));
            write_plugin_hooks(&workspace, BridgeTarget::Codex, mode, force, &mut files)?;
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

fn write_plugin_hooks(
    workspace: &Path,
    target: BridgeTarget,
    mode: PluginMode,
    force: bool,
    files: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let hooks_root = match target {
        BridgeTarget::ClaudeCode => workspace.join(".claude").join("hooks"),
        BridgeTarget::Codex => workspace.join(".codex").join("hooks"),
        BridgeTarget::Kimetsu => workspace.join(".kimetsu").join("hooks"),
    };
    let ext = hook_file_extension();
    let script = kimetsu_hook_script(mode);
    for event in ["pre-turn", "post-turn"] {
        let hook = hooks_root.join(format!("{event}.{ext}"));
        write_text_file(&hook, &script, force)?;
        files.push(normalize_path(&hook));
    }
    Ok(())
}

fn hook_file_extension() -> &'static str {
    if cfg!(windows) { "ps1" } else { "sh" }
}

fn kimetsu_hook_script(mode: PluginMode) -> String {
    let template = if cfg!(windows) {
        KIMETSU_HOOK_PS1_TEMPLATE
    } else {
        KIMETSU_HOOK_SH_TEMPLATE
    };
    template.replace("__KIMETSU_MODE__", mode.as_str())
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

    #[test]
    fn plugin_install_writes_optional_and_required_modes() {
        let root = temp_root("plugin_install_modes");

        let optional = plugin_install(&root, BridgeTarget::Codex, PluginMode::Optional, false)
            .expect("optional install");
        assert_eq!(optional.mode, PluginMode::Optional);
        let skill_path = root.join(".codex/skills/kimetsu-bridge/SKILL.md");
        let optional_text = fs::read_to_string(&skill_path).expect("optional skill");
        assert!(optional_text.contains("Optional mode"));
        assert!(optional_text.contains("kimetsu_brain_context"));
        assert!(optional_text.contains("kimetsu_benchmark_context"));
        assert!(optional_text.contains("kimetsu_benchmark_record_outcome"));
        assert!(!optional_text.contains("kimetsu_harbor"));
        let hook_ext = hook_file_extension();
        let pre_turn_hook = root.join(format!(".codex/hooks/pre-turn.{hook_ext}"));
        let post_turn_hook = root.join(format!(".codex/hooks/post-turn.{hook_ext}"));
        assert!(pre_turn_hook.is_file());
        assert!(post_turn_hook.is_file());
        let optional_hook = fs::read_to_string(&pre_turn_hook).expect("optional hook");
        assert!(optional_hook.contains("optional"));
        assert!(optional_hook.contains("brain context"));

        let required = plugin_install(&root, BridgeTarget::Codex, PluginMode::Required, true)
            .expect("required install");
        assert_eq!(required.mode, PluginMode::Required);
        let required_text = fs::read_to_string(&skill_path).expect("required skill");
        assert!(required_text.contains("Required mode"));
        assert!(required_text.contains("setup blocker"));
        assert!(required_text.contains("kimetsu_benchmark_context"));
        assert!(required_text.contains("kimetsu_benchmark_record_outcome"));
        assert!(!required_text.contains("kimetsu_harbor"));
        let required_hook = fs::read_to_string(&pre_turn_hook).expect("required hook");
        assert!(required_hook.contains("required"));
        assert!(required_hook.contains("kimetsu brain context"));
        assert!(required.files.iter().any(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("pre-turn."))
                .unwrap_or(false)
        }));

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
