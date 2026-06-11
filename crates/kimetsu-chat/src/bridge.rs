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
    Cursor,
    GeminiCli,
    #[cfg(feature = "openclaw")]
    OpenClaw,
    #[cfg(feature = "pi")]
    Pi,
}

impl BridgeTarget {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            "kimetsu" => Ok(Self::Kimetsu),
            "cursor" => Ok(Self::Cursor),
            "gemini" | "gemini-cli" => Ok(Self::GeminiCli),
            #[cfg(feature = "openclaw")]
            "openclaw" | "claw" => Ok(Self::OpenClaw),
            #[cfg(not(feature = "openclaw"))]
            "openclaw" | "claw" => {
                Err("this build was compiled without the OpenClaw integration; \
                 reinstall with `--features openclaw`"
                    .to_string())
            }
            #[cfg(feature = "pi")]
            "pi" => Ok(Self::Pi),
            #[cfg(not(feature = "pi"))]
            "pi" => Err("this build was compiled without the Pi integration; \
                 reinstall with `--features pi`"
                .to_string()),
            other => Err(format!("unknown bridge target `{other}`")),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Kimetsu => "kimetsu",
            Self::Cursor => "cursor",
            Self::GeminiCli => "gemini-cli",
            #[cfg(feature = "openclaw")]
            Self::OpenClaw => "openclaw",
            #[cfg(feature = "pi")]
            Self::Pi => "pi",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PluginMode {
    #[default]
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

/// Where the plugin surface is installed: the current workspace
/// (`.claude/`, `.codex/`, `.mcp.json`) or the user's home directory
/// (`~/.claude/`, `~/.claude.json`, `~/.codex/`) for all sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InstallScope {
    #[default]
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
    /// Informational notes surfaced to the user (e.g. format changes during install).
    pub notes: Vec<String>,
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

#[cfg(feature = "pi")]
/// TypeScript extension installed at `<pi_dir>/extensions/kimetsu.ts`.
///
/// Pi extensions are auto-discovered from `~/.pi/agent/extensions/` (global)
/// or `.pi/extensions/` (project). No MCP is available; Kimetsu integrates
/// via `pi.exec()` on Pi's lifecycle events. The extension **silently no-ops**
/// when `kimetsu` is not on PATH — a missing binary must never break Pi.
const PI_EXTENSION_TS: &str = r#"// Kimetsu brain extension for Pi (earendil-works/pi).
// Auto-generated by `kimetsu plugin install pi` — do not edit by hand.
//
// Shells out to the kimetsu binary on Pi lifecycle events to load brain
// context at session start and record audit markers on session end.
// If kimetsu is not on PATH the exec silently fails; Pi startup is unaffected.

import { spawn } from "node:child_process";

function kimetsuExec(args: string[]): Promise<void> {
  return new Promise((resolve) => {
    try {
      const child = spawn("kimetsu", args, {
        stdio: "ignore",
        shell: false,
        windowsHide: true,
      });
      child.on("error", () => resolve()); // binary not on PATH — silent no-op
      child.on("close", () => resolve());
    } catch {
      resolve(); // any unexpected error — silent no-op
    }
  });
}

export default function (pi: any) {
  // session_start fires once when Pi starts up or a new session begins.
  pi.on("session_start", async (_event: any, _ctx: any) => {
    await kimetsuExec(["brain", "warm"]);
    await kimetsuExec(["brain", "context-hook"]);
  });

  // agent_end fires after the LLM turn completes (maps to Kimetsu stop-hook).
  pi.on("agent_end", async (_event: any, _ctx: any) => {
    await kimetsuExec(["brain", "stop-hook"]);
  });

  // session_shutdown fires on clean session close (maps to session-end-hook).
  pi.on("session_shutdown", async (_event: any, _ctx: any) => {
    await kimetsuExec(["brain", "session-end-hook"]);
  });
}
"#;

#[cfg(feature = "pi")]
/// SKILL.md installed at `<pi_dir>/skills/kimetsu-brain/SKILL.md`.
///
/// Pi skills are plain Markdown with optional YAML frontmatter. No MCP is
/// available in Pi, so the skill describes the brain commands the agent can
/// shell out to via `pi.exec()` or custom tools if wired.
const PI_SKILL_MD: &str = r#"---
name: kimetsu-brain
description: Use Kimetsu brain shell commands as a persistent memory sidecar across Pi sessions.
---
Kimetsu is a persistent brain sidecar accessible via the `kimetsu` CLI. Use it
when the task may benefit from prior session knowledge, workflow memory, or
durable cross-session context.

Brain-first workflow:
1. Before planning or editing broad coding, review, debugging, or setup tasks,
   run `kimetsu brain context <query>` and read the returned capsules as working
   context before deciding on a plan.
2. After solving a non-obvious problem, run `kimetsu brain record` with a
   concrete, actionable lesson and 2-5 domain tags so future sessions benefit.
3. Run `kimetsu brain status` when you need to know whether the brain is
   initialized, has accepted memories, or has pending proposals.

Optional mode: Kimetsu brain context is a preferred first step for non-trivial
work. If the binary is unavailable, note the absence and continue normally.
"#;

#[cfg(feature = "openclaw")]
/// TypeScript plugin installed at `<oc_dir>/plugins/kimetsu/index.ts`.
///
/// OpenClaw plugins are discovered from `plugins/<id>/` with an
/// `openclaw.plugin.json` manifest and a TypeScript entry point. The plugin
/// uses `api.on(event, handler)` to hook lifecycle events. It **silently
/// no-ops** when `kimetsu` is not on PATH — a missing binary must never break
/// OpenClaw startup.
///
/// Verified real event names from docs/plugins/hooks.md:
///   - `agent_turn_prepare` — fires before each agent turn begins (maps to context-hook)
///   - `agent_end`          — fires after each turn completes (maps to stop-hook)
///   - `session_end`        — fires on clean session close (maps to session-end-hook)
const OPENCLAW_PLUGIN_TS: &str = r#"// Kimetsu brain plugin for OpenClaw (openclaw/openclaw).
// Auto-generated by `kimetsu plugin install openclaw` — do not edit by hand.
//
// Hooks OpenClaw lifecycle events to load Kimetsu brain context at the start
// of each agent turn and record audit markers when the turn or session ends.
// If kimetsu is not on PATH the spawn silently fails; OpenClaw is unaffected.

import { spawn } from "node:child_process";
import { definePluginEntry } from "openclaw/plugin-sdk/plugin-entry";

function kimetsuExec(args: string[]): Promise<void> {
  return new Promise((resolve) => {
    try {
      const child = spawn("kimetsu", args, {
        stdio: "ignore",
        shell: false,
        windowsHide: true,
      });
      child.on("error", () => resolve()); // binary not on PATH — silent no-op
      child.on("close", () => resolve());
    } catch {
      resolve(); // any unexpected error — silent no-op
    }
  });
}

export default definePluginEntry({
  register(api: any) {
    // Warm the embedder daemon at plugin registration (startup).
    kimetsuExec(["brain", "warm"]);

    // agent_turn_prepare fires before each turn: load brain context.
    api.on("agent_turn_prepare", async (_ctx: any) => {
      await kimetsuExec(["brain", "context-hook"]);
    });

    // agent_end fires after each turn: record audit marker / nudge memory.
    api.on("agent_end", async (_ctx: any) => {
      await kimetsuExec(["brain", "stop-hook"]);
    });

    // session_end fires on clean session close.
    api.on("session_end", async (_ctx: any) => {
      await kimetsuExec(["brain", "session-end-hook"]);
    });
  },
});
"#;

#[cfg(feature = "openclaw")]
/// Plugin manifest installed at `<oc_dir>/plugins/kimetsu/openclaw.plugin.json`.
///
/// OpenClaw uses this file to discover plugin identity and capabilities.
/// The `activation.onStartup` flag ensures the plugin is loaded immediately
/// when OpenClaw starts, so hooks are registered before any agent turn.
const OPENCLAW_PLUGIN_MANIFEST: &str = r#"{
  "id": "kimetsu",
  "name": "Kimetsu Brain",
  "description": "Persistent memory brain sidecar — loads context on each agent turn and records audit markers on stop/session-end.",
  "contracts": {},
  "activation": {
    "onStartup": true
  }
}
"#;

#[cfg(feature = "openclaw")]
/// SKILL.md installed at `<oc_dir>/workspace/skills/kimetsu-context/SKILL.md`.
///
/// OpenClaw workspace skills live in `~/.openclaw/workspace/skills/<skill>/SKILL.md`
/// and are loaded as agent guidance during workspace initialization. They use
/// plain Markdown with optional YAML frontmatter.
const OPENCLAW_SKILL_MD: &str = r#"---
name: kimetsu-context
description: Use Kimetsu MCP tools as a persistent brain sidecar across OpenClaw sessions.
---
Kimetsu is a persistent memory brain accessible via the `kimetsu` MCP server
(registered in your `openclaw.json` as `mcp.servers.kimetsu`). Use it when the
task may benefit from prior session knowledge, workflow memory, or durable
cross-session context.

Brain-first workflow:
1. Before planning or editing broad coding, review, debugging, or setup tasks,
   call `kimetsu_brain_context` with a concise query and use the returned
   capsules as working context before deciding on a plan.
2. After solving a non-obvious problem, call `kimetsu_brain_record` with a
   concrete, actionable lesson and 2-5 domain tags so future sessions benefit.
3. Call `kimetsu_brain_status` when you need to know whether the brain is
   initialized, has accepted memories, or has pending proposals.

Optional mode: Kimetsu brain context is a preferred first step for non-trivial
work. If the MCP server is unavailable, note the absence and continue normally.
"#;

/// Kimetsu brain guidance installed in `.cursor/rules/kimetsu-brain/rule.md`.
///
/// Cursor reads rules from `.cursor/rules/<name>/rule.md` (or `.cursor/rules/`
/// directly as `.mdc` files in older versions). The `alwaysApply: true` front-
/// matter ensures the guidance is active in every chat session without requiring
/// the user to invoke it manually.
///
/// Cursor has NO `UserPromptSubmit`-style hooks: MCP tools plus this always-on
/// rule are the only integration surfaces.
///
/// Source: https://cursor.com/docs/mcp and https://cursor.com/docs/rules
const CURSOR_RULES_MD: &str = r#"---
description: Use Kimetsu persistent brain MCP tools as a sidecar for this workspace.
alwaysApply: true
---
# Kimetsu brain

You have a persistent memory brain attached via MCP (tools prefixed `kimetsu_`).

- **Before non-trivial tasks**: call `kimetsu_brain_context` with a short query. If the brain
  has relevant prior knowledge it will return it. If not (`skipped: true`), proceed as normal —
  this is zero overhead.
- **After solving a non-obvious problem**: call `kimetsu_brain_record` with what you learned
  and 2-5 domain tags. Keep lessons concrete and actionable, not platitudes.

Do not call either tool on simple/one-liner tasks. The brain is for things that required real
effort or that you would want to remember next session.
"#;

/// GEMINI.md guidance installed in the project root (workspace install) or
/// `~/.gemini/GEMINI.md` (global install).
///
/// Gemini CLI discovers GEMINI.md files from the project root and parent dirs
/// up to the git root, as well as `~/.gemini/GEMINI.md` for global context.
/// All discovered files are concatenated and sent with every prompt.
///
/// Gemini CLI has no hook system — MCP + GEMINI.md is the complete integration
/// surface.
///
/// Source: https://google-gemini.github.io/gemini-cli/docs/cli/gemini-md.html
const GEMINI_MD_CONTENT: &str = r#"# Kimetsu brain

You have a persistent memory brain attached via MCP (tools prefixed `kimetsu_`).

- **Before non-trivial tasks**: call `kimetsu_brain_context` with a short query. If the brain
  has relevant prior knowledge it will return it. If not (`skipped: true`), proceed as normal —
  this is zero overhead.
- **After solving a non-obvious problem**: call `kimetsu_brain_record` with what you learned
  and 2-5 domain tags. Keep lessons concrete and actionable, not platitudes.

Do not call either tool on simple/one-liner tasks. The brain is for things that required real
effort or that you would want to remember next session.
"#;

const GEMINI_MD_BEGIN: &str = "<!-- kimetsu:begin -->";
const GEMINI_MD_END: &str = "<!-- kimetsu:end -->";

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
        BridgeTarget::Cursor => workspace.join(".cursor").join("skills").join(&name),
        BridgeTarget::GeminiCli => workspace.join(".gemini").join("skills").join(&name),
        #[cfg(feature = "openclaw")]
        BridgeTarget::OpenClaw => workspace
            .join(".openclaw")
            .join("workspace")
            .join("skills")
            .join(&name),
        #[cfg(feature = "pi")]
        BridgeTarget::Pi => workspace.join(".pi").join("skills").join(&name),
    };
    let destination_parent = destination_root
        .parent()
        .ok_or_else(|| format!("{} has no parent", destination_root.display()))?;
    copy_dir_with_replace(&source_root, &destination_root, destination_parent, force)?;
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

/// Remote-server wiring parameters for [`plugin_install_remote`].
#[derive(Debug, Clone)]
pub struct RemoteInstall {
    /// Server base URL, e.g. `https://kimetsu.example.com:8787` (no `/mcp/...`).
    pub base_url: String,
    /// Sanitized repo id; the endpoint becomes `<base_url>/mcp/<repo_id>`.
    pub repo_id: String,
    /// Literal bearer token; `None` writes a `${KIMETSU_REMOTE_TOKEN}` reference
    /// so the secret never lands on disk.
    pub token: Option<String>,
}

/// Wire a host to a REMOTE Kimetsu server (HTTP MCP) instead of the local stdio
/// command: writes a `url`+`Authorization` MCP entry plus brain-usage guidance,
/// and no local hooks (the brain lives on the server). Supported for Claude Code
/// and OpenClaw — the hosts with remote-MCP support.
pub fn plugin_install_remote(
    workspace: &Path,
    target: BridgeTarget,
    scope: InstallScope,
    mode: PluginMode,
    remote: &RemoteInstall,
) -> Result<PluginInstallReport, String> {
    let home = match scope {
        InstallScope::Global => Some(resolve_home()?),
        InstallScope::Workspace => None,
    };
    plugin_install_remote_inner(workspace, target, scope, mode, remote, home.as_deref())
}

fn plugin_install_remote_inner(
    workspace: &Path,
    target: BridgeTarget,
    scope: InstallScope,
    mode: PluginMode,
    remote: &RemoteInstall,
    home: Option<&Path>,
) -> Result<PluginInstallReport, String> {
    let workspace = normalize_path(workspace);
    let endpoint = format!(
        "{}/mcp/{}",
        remote.base_url.trim_end_matches('/'),
        remote.repo_id
    );
    let auth = format!(
        "Bearer {}",
        remote
            .token
            .clone()
            .unwrap_or_else(|| "${KIMETSU_REMOTE_TOKEN}".to_string())
    );
    let mut files = Vec::new();
    let mut notes = Vec::new();
    match target {
        BridgeTarget::ClaudeCode => {
            let server = serde_json::json!({
                "type": "http",
                "url": endpoint,
                "headers": { "Authorization": auth }
            });
            let (mcp, only) = match home {
                Some(h) => (h.join(".claude.json"), true),
                None => (workspace.join(".mcp.json"), false),
            };
            write_mcp_config_server(&mcp, only, server)?;
            files.push(normalize_path(&mcp));

            let claude_dir = match home {
                Some(h) => h.join(".claude"),
                None => workspace.join(".claude"),
            };
            fs::create_dir_all(&claude_dir)
                .map_err(|err| format!("create {}: {err}", claude_dir.display()))?;
            let claude_md = claude_dir.join("CLAUDE.md");
            merge_claude_md(&claude_md)?;
            files.push(normalize_path(&claude_md));
        }
        #[cfg(feature = "openclaw")]
        BridgeTarget::OpenClaw => {
            let server = serde_json::json!({
                "url": endpoint,
                "transport": "streamable-http",
                "headers": { "Authorization": auth }
            });
            let oc_dir = match home {
                Some(h) => h.join(".openclaw"),
                None => workspace.join(".openclaw"),
            };
            fs::create_dir_all(&oc_dir)
                .map_err(|err| format!("create {}: {err}", oc_dir.display()))?;
            let oc_json = oc_dir.join("openclaw.json");
            write_openclaw_remote_mcp(&oc_json, server, &mut notes)?;
            files.push(normalize_path(&oc_json));
            let skill = oc_dir
                .join("skills")
                .join("kimetsu-context")
                .join("SKILL.md");
            write_text_file(&skill, OPENCLAW_SKILL_MD, true)?;
            files.push(normalize_path(&skill));
        }
        other => {
            return Err(format!(
                "remote install is supported for claude-code and openclaw, not `{}`",
                other.as_str()
            ));
        }
    }
    notes.push(format!("remote brain endpoint: {endpoint}"));
    if remote.token.is_none() {
        notes.push(
            "auth reads ${KIMETSU_REMOTE_TOKEN} — set that env var where your host agent runs"
                .to_string(),
        );
    }
    Ok(PluginInstallReport {
        target,
        scope,
        mode,
        files,
        notes,
    })
}

/// Upsert only `mcp.servers.kimetsu` in an OpenClaw config with a remote server
/// value (no hooks plugin — the brain is remote).
#[cfg(feature = "openclaw")]
fn write_openclaw_remote_mcp(
    path: &Path,
    server: serde_json::Value,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let had_file = path.is_file();
    let mut root = if had_file {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        json5::from_str::<serde_json::Value>(&text)
            .map_err(|err| format!("parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({})
    };
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must be a JSON object", path.display()))?;
    let mcp = root_obj
        .entry("mcp".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let mcp_obj = mcp
        .as_object_mut()
        .ok_or_else(|| format!("{} `mcp` must be a JSON object", path.display()))?;
    let servers = mcp_obj
        .entry("servers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| format!("{} `mcp.servers` must be a JSON object", path.display()))?;
    servers_obj.insert("kimetsu".to_string(), server);
    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    if had_file {
        notes.push(
            "note: openclaw.json was reformatted (JSON5 comments are not preserved)".to_string(),
        );
    }
    write_text_file(path, &text, true)
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
    #[allow(unused_mut)]
    let mut notes: Vec<String> = Vec::new();
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

        BridgeTarget::Cursor => {
            // Cursor: MCP config in .cursor/mcp.json (workspace) or
            // ~/.cursor/mcp.json (global), key `mcpServers`.
            // No hooks available in Cursor — wire MCP + always-on rules file only.
            //
            // Config schema verified from https://cursor.com/docs/mcp (June 2026):
            //   mcpServers.<name>.type = "stdio"
            //   mcpServers.<name>.command = "kimetsu"
            //   mcpServers.<name>.args = ["mcp", "serve", "--workspace", "."]
            let cursor_dir = match home {
                Some(h) => h.join(".cursor"),
                None => workspace.join(".cursor"),
            };
            fs::create_dir_all(&cursor_dir)
                .map_err(|err| format!("create {}: {err}", cursor_dir.display()))?;
            let mcp = cursor_dir.join("mcp.json");
            write_cursor_mcp_config(&mcp)?;
            files.push(normalize_path(&mcp));

            // Rules file: .cursor/rules/kimetsu-brain/rule.md
            // (workspace install only; global Cursor config does not support rules files)
            if home.is_none() {
                let rule = workspace
                    .join(".cursor")
                    .join("rules")
                    .join("kimetsu-brain")
                    .join("rule.md");
                write_text_file(&rule, CURSOR_RULES_MD, true)?;
                files.push(normalize_path(&rule));
            }
        }

        BridgeTarget::GeminiCli => {
            // Gemini CLI: MCP config in .gemini/settings.json (workspace) or
            // ~/.gemini/settings.json (global), key `mcpServers`.
            // No hook system — wire MCP + GEMINI.md context file only.
            //
            // Config schema verified from google-gemini/gemini-cli docs (June 2026):
            //   mcpServers.<name>.command = "kimetsu"
            //   mcpServers.<name>.args = ["mcp", "serve", "--workspace", "."]
            // GEMINI.md: project root (workspace) or ~/.gemini/GEMINI.md (global)
            let gemini_dir = match home {
                Some(h) => h.join(".gemini"),
                None => workspace.join(".gemini"),
            };
            fs::create_dir_all(&gemini_dir)
                .map_err(|err| format!("create {}: {err}", gemini_dir.display()))?;
            let settings = gemini_dir.join("settings.json");
            write_gemini_settings(&settings)?;
            files.push(normalize_path(&settings));

            // GEMINI.md: merge into project root (workspace) or ~/.gemini/GEMINI.md (global)
            let gemini_md_path = match home {
                Some(h) => h.join(".gemini").join("GEMINI.md"),
                None => workspace.join("GEMINI.md"),
            };
            merge_gemini_md(&gemini_md_path)?;
            files.push(normalize_path(&gemini_md_path));
        }

        #[cfg(feature = "openclaw")]
        BridgeTarget::OpenClaw => {
            // OpenClaw supports MCP natively.
            // Global → ~/.openclaw/; Workspace → <workspace>/.openclaw/.
            let oc_dir = match home {
                Some(h) => h.join(".openclaw"),
                None => workspace.join(".openclaw"),
            };

            // openclaw.json — upsert mcp.servers.kimetsu + plugins.entries.kimetsu.
            let config = oc_dir.join("openclaw.json");
            write_openclaw_config(&config, &mut notes)?;
            files.push(normalize_path(&config));

            // plugins/kimetsu/index.ts
            let plugin_ts = oc_dir.join("plugins").join("kimetsu").join("index.ts");
            write_text_file(&plugin_ts, OPENCLAW_PLUGIN_TS, true)?;
            files.push(normalize_path(&plugin_ts));

            // plugins/kimetsu/openclaw.plugin.json
            let plugin_manifest = oc_dir
                .join("plugins")
                .join("kimetsu")
                .join("openclaw.plugin.json");
            write_text_file(&plugin_manifest, OPENCLAW_PLUGIN_MANIFEST, true)?;
            files.push(normalize_path(&plugin_manifest));

            // workspace/skills/kimetsu-context/SKILL.md
            let skill = oc_dir
                .join("workspace")
                .join("skills")
                .join("kimetsu-context")
                .join("SKILL.md");
            write_text_file(&skill, OPENCLAW_SKILL_MD, true)?;
            files.push(normalize_path(&skill));
        }

        #[cfg(feature = "pi")]
        BridgeTarget::Pi => {
            // Pi has no MCP. Kimetsu integrates via a TS extension + a SKILL.md.
            // Global → ~/.pi/agent/; Workspace → .pi/ (project-local config).
            let pi_dir = match home {
                Some(h) => h.join(".pi").join("agent"),
                None => workspace.join(".pi"),
            };

            // extensions/kimetsu.ts
            let ext_file = pi_dir.join("extensions").join("kimetsu.ts");
            write_text_file(&ext_file, PI_EXTENSION_TS, true)?;
            files.push(normalize_path(&ext_file));

            // settings.json — idempotently register the extension.
            let settings = pi_dir.join("settings.json");
            write_pi_settings(&settings)?;
            files.push(normalize_path(&settings));

            // skills/kimetsu-brain/SKILL.md
            let skill = pi_dir.join("skills").join("kimetsu-brain").join("SKILL.md");
            write_text_file(&skill, PI_SKILL_MD, true)?;
            files.push(normalize_path(&skill));
        }
    }
    Ok(PluginInstallReport {
        target,
        scope,
        mode,
        files,
        notes,
    })
}

pub fn extensions_root(workspace: &Path) -> PathBuf {
    workspace.join(".kimetsu").join("extensions")
}

// ---------------------------------------------------------------------------
// plugin_status — read-only wiring detector
// ---------------------------------------------------------------------------

/// Overall wiring state for one host+scope combination.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WiringState {
    /// All core pieces (hooks + mcp) are present.
    Installed,
    /// Some but not all expected pieces are present.
    Partial,
    /// No Kimetsu wiring found.
    Absent,
}

/// Status of Kimetsu's wiring for a specific host+scope.
#[derive(Debug, Clone, Serialize)]
pub struct PluginScopeStatus {
    /// "claude-code" or "codex"
    pub host: String,
    /// "workspace" or "global"
    pub scope: String,
    pub state: WiringState,
    /// Which pieces are present (e.g. "hooks", "mcp", "CLAUDE.md", "commands", "agent").
    pub present: Vec<String>,
    /// Expected-but-absent pieces (populated when state is Partial).
    pub missing: Vec<String>,
    /// Primary config dir/file for this host+scope.
    pub config_path: String,
}

// ── per-piece detection helpers ────────────────────────────────────────────

/// Returns true if `settings.json` exists and has at least one Kimetsu hook group.
fn detect_claude_hooks(claude_dir: &Path) -> bool {
    let settings = claude_dir.join("settings.json");
    if !settings.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&settings) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(strip_bom(&text)) else {
        return false;
    };
    root.get("hooks")
        .and_then(|h| h.as_object())
        .map(|hooks_obj| {
            hooks_obj.values().any(|event_val| {
                event_val
                    .as_array()
                    .map(|groups| groups.iter().any(is_kimetsu_hook_group))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Returns true if the MCP config file has a `"kimetsu"` key in
/// `mcpServers` (always checked) or `servers` (only when `check_servers` is true).
fn detect_claude_mcp(mcp_path: &Path, check_servers: bool) -> bool {
    if !mcp_path.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(mcp_path) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(strip_bom(&text)) else {
        return false;
    };
    let in_mcp_servers = root
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key("kimetsu"))
        .unwrap_or(false);
    let in_servers = if check_servers {
        root.get("servers")
            .and_then(|v| v.as_object())
            .map(|m| m.contains_key("kimetsu"))
            .unwrap_or(false)
    } else {
        false
    };
    in_mcp_servers || in_servers
}

/// Returns true if CLAUDE.md contains the Kimetsu begin marker.
fn detect_claude_md(claude_dir: &Path) -> bool {
    let md = claude_dir.join("CLAUDE.md");
    if !md.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&md) else {
        return false;
    };
    text.contains(CLAUDE_MD_BEGIN)
}

/// Returns true if `commands/kimetsu/` directory exists under `claude_dir`.
fn detect_claude_commands(claude_dir: &Path) -> bool {
    claude_dir.join("commands").join("kimetsu").is_dir()
}

/// Returns true if `agents/kimetsu-memory-harvester.md` exists under `claude_dir`.
fn detect_claude_agent(claude_dir: &Path) -> bool {
    claude_dir
        .join("agents")
        .join("kimetsu-memory-harvester.md")
        .is_file()
}

/// Returns true if `config.toml` has `[mcp_servers.kimetsu]`.
fn detect_codex_mcp(codex_dir: &Path) -> bool {
    let config = codex_dir.join("config.toml");
    if !config.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&config) else {
        return false;
    };
    let Ok(root) = toml::from_str::<toml::Value>(strip_bom(&text)) else {
        return false;
    };
    root.get("mcp_servers")
        .and_then(|v| v.as_table())
        .map(|t| t.contains_key("kimetsu"))
        .unwrap_or(false)
}

/// Returns true if `hooks.json` has at least one Kimetsu hook group.
fn detect_codex_hooks(codex_dir: &Path) -> bool {
    // Codex hooks.json uses the same `{ "hooks": { … } }` structure as
    // Claude's settings.json. Check codex_dir/hooks.json directly.
    let hooks = codex_dir.join("hooks.json");
    if !hooks.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&hooks) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(strip_bom(&text)) else {
        return false;
    };
    root.get("hooks")
        .and_then(|h| h.as_object())
        .map(|hooks_obj| {
            hooks_obj.values().any(|event_val| {
                event_val
                    .as_array()
                    .map(|groups| groups.iter().any(is_kimetsu_hook_group))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Returns true if `skills/kimetsu-bridge/` exists under `codex_dir`.
fn detect_codex_skill(codex_dir: &Path) -> bool {
    codex_dir.join("skills").join("kimetsu-bridge").is_dir()
}

/// Returns true if `agents/kimetsu-memory-harvester.toml` exists under `codex_dir`.
fn detect_codex_agent(codex_dir: &Path) -> bool {
    codex_dir
        .join("agents")
        .join("kimetsu-memory-harvester.toml")
        .is_file()
}

/// Returns true if `.cursor/mcp.json` has `mcpServers.kimetsu`.
fn detect_cursor_mcp(cursor_dir: &Path) -> bool {
    let mcp = cursor_dir.join("mcp.json");
    if !mcp.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&mcp) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(strip_bom(&text)) else {
        return false;
    };
    root.get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key("kimetsu"))
        .unwrap_or(false)
}

/// Returns true if `.cursor/rules/kimetsu-brain/` directory exists in `workspace`.
fn detect_cursor_rules(workspace: &Path) -> bool {
    workspace
        .join(".cursor")
        .join("rules")
        .join("kimetsu-brain")
        .is_dir()
}

/// Returns true if `.gemini/settings.json` has `mcpServers.kimetsu`.
fn detect_gemini_mcp(gemini_dir: &Path) -> bool {
    let settings = gemini_dir.join("settings.json");
    if !settings.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&settings) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(strip_bom(&text)) else {
        return false;
    };
    root.get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key("kimetsu"))
        .unwrap_or(false)
}

/// Returns true if `GEMINI.md` in the workspace root has the Kimetsu begin marker.
fn detect_gemini_md(workspace: &Path) -> bool {
    let md = workspace.join("GEMINI.md");
    if !md.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&md) else {
        return false;
    };
    text.contains(GEMINI_MD_BEGIN)
}

/// Returns true if `~/.gemini/GEMINI.md` (global) has the Kimetsu begin marker.
fn detect_gemini_global_md(gemini_dir: &Path) -> bool {
    let md = gemini_dir.join("GEMINI.md");
    if !md.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&md) else {
        return false;
    };
    text.contains(GEMINI_MD_BEGIN)
}

#[cfg(feature = "pi")]
/// Returns true if Pi's `settings.json` registers the kimetsu extension AND
/// `extensions/kimetsu.ts` exists in `pi_dir`.
fn detect_pi_extension(pi_dir: &Path) -> bool {
    let ext_file = pi_dir.join("extensions").join("kimetsu.ts");
    if !ext_file.is_file() {
        return false;
    }
    let settings = pi_dir.join("settings.json");
    if !settings.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&settings) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(strip_bom(&text)) else {
        return false;
    };
    root.get("extensions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|v| v.as_str() == Some("./extensions/kimetsu.ts"))
        })
        .unwrap_or(false)
}

#[cfg(feature = "pi")]
/// Returns true if `skills/kimetsu-brain/` exists under `pi_dir`.
fn detect_pi_skill(pi_dir: &Path) -> bool {
    pi_dir.join("skills").join("kimetsu-brain").is_dir()
}

#[cfg(feature = "openclaw")]
/// Returns true if `openclaw.json` has `mcp.servers.kimetsu`.
fn detect_openclaw_mcp(oc_dir: &Path) -> bool {
    let config = oc_dir.join("openclaw.json");
    if !config.is_file() {
        return false;
    }
    let Ok(text) = fs::read_to_string(&config) else {
        return false;
    };
    let Ok(root) = json5::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    root.get("mcp")
        .and_then(|v| v.get("servers"))
        .and_then(|v| v.as_object())
        .map(|m| m.contains_key("kimetsu"))
        .unwrap_or(false)
}

#[cfg(feature = "openclaw")]
/// Returns true if `plugins/kimetsu/` directory exists (with the manifest) under `oc_dir`.
fn detect_openclaw_plugin(oc_dir: &Path) -> bool {
    oc_dir
        .join("plugins")
        .join("kimetsu")
        .join("openclaw.plugin.json")
        .is_file()
}

#[cfg(feature = "openclaw")]
/// Returns true if `workspace/skills/kimetsu-context/` directory exists under `oc_dir`.
fn detect_openclaw_skill(oc_dir: &Path) -> bool {
    oc_dir
        .join("workspace")
        .join("skills")
        .join("kimetsu-context")
        .is_dir()
}

// ── state aggregation ───────────────────────────────────────────────────────

/// Aggregate present/missing into a `WiringState`.
///
/// Core pieces are: `"hooks"`, `"mcp"`, `"extension"`, `"plugin"`.
/// If all pieces are present → `Installed`.
/// If any core piece is present but something is missing → `Partial`.
/// If no core piece is present at all → `Absent`.
fn aggregate_state(present: &[&str], missing: &[&str]) -> WiringState {
    if missing.is_empty() {
        WiringState::Installed
    } else if present.is_empty() {
        WiringState::Absent
    } else {
        let has_core = present
            .iter()
            .any(|p| matches!(*p, "hooks" | "mcp" | "extension" | "plugin"));
        if has_core {
            WiringState::Partial
        } else {
            WiringState::Absent
        }
    }
}

/// Read-only status scan: check each (host, scope) combination and report
/// which Kimetsu wiring pieces are present, missing, or absent.
///
/// For workspace scope the `home` parameter is `None`; for global it is
/// `Some(&home_dir)` — mirroring `plugin_install_inner`/`plugin_uninstall_inner`.
fn plugin_status_inner(workspace: &Path) -> Vec<PluginScopeStatus> {
    let workspace = normalize_path(workspace);
    let mut results = Vec::new();

    let home_opt = resolve_home().ok();

    #[allow(unused_mut)]
    let mut scan_targets = vec![
        BridgeTarget::ClaudeCode,
        BridgeTarget::Codex,
        BridgeTarget::Cursor,
        BridgeTarget::GeminiCli,
    ];
    #[cfg(feature = "openclaw")]
    scan_targets.push(BridgeTarget::OpenClaw);
    #[cfg(feature = "pi")]
    scan_targets.push(BridgeTarget::Pi);

    for &target in &scan_targets {
        for &scope in &[InstallScope::Workspace, InstallScope::Global] {
            let home: Option<&Path> = match scope {
                InstallScope::Global => {
                    match home_opt.as_deref() {
                        Some(h) => Some(h),
                        None => {
                            // Can't resolve home — report this scope as Absent.
                            results.push(PluginScopeStatus {
                                host: target.as_str().to_string(),
                                scope: scope.as_str().to_string(),
                                state: WiringState::Absent,
                                present: vec![],
                                missing: vec![],
                                config_path: "(home unavailable)".to_string(),
                            });
                            continue;
                        }
                    }
                }
                InstallScope::Workspace => None,
            };

            match target {
                BridgeTarget::ClaudeCode => {
                    let claude_dir = match home {
                        Some(h) => h.join(".claude"),
                        None => workspace.join(".claude"),
                    };
                    let mcp_path = match home {
                        Some(h) => h.join(".claude.json"),
                        None => workspace.join(".mcp.json"),
                    };
                    // `servers` key is only in workspace .mcp.json, not global ~/.claude.json
                    let check_servers = home.is_none();

                    let hooks_ok = detect_claude_hooks(&claude_dir);
                    let mcp_ok = detect_claude_mcp(&mcp_path, check_servers);
                    let claude_md_ok = detect_claude_md(&claude_dir);
                    let commands_ok = detect_claude_commands(&claude_dir);
                    let agent_ok = detect_claude_agent(&claude_dir);

                    let mut present = Vec::new();
                    let mut missing = Vec::new();

                    for (name, ok) in [
                        ("hooks", hooks_ok),
                        ("mcp", mcp_ok),
                        ("CLAUDE.md", claude_md_ok),
                        ("commands", commands_ok),
                        ("agent", agent_ok),
                    ] {
                        if ok {
                            present.push(name.to_string());
                        } else {
                            missing.push(name.to_string());
                        }
                    }

                    let state = aggregate_state(
                        &present.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &missing.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    );

                    results.push(PluginScopeStatus {
                        host: target.as_str().to_string(),
                        scope: scope.as_str().to_string(),
                        state,
                        present,
                        missing,
                        config_path: claude_dir.to_string_lossy().to_string(),
                    });
                }

                BridgeTarget::Codex => {
                    let codex_dir = match home {
                        Some(h) => h.join(".codex"),
                        None => workspace.join(".codex"),
                    };

                    let hooks_ok = detect_codex_hooks(&codex_dir);
                    let mcp_ok = detect_codex_mcp(&codex_dir);
                    let skill_ok = detect_codex_skill(&codex_dir);
                    let agent_ok = detect_codex_agent(&codex_dir);

                    let mut present = Vec::new();
                    let mut missing = Vec::new();

                    for (name, ok) in [
                        ("hooks", hooks_ok),
                        ("mcp", mcp_ok),
                        ("skill", skill_ok),
                        ("agent", agent_ok),
                    ] {
                        if ok {
                            present.push(name.to_string());
                        } else {
                            missing.push(name.to_string());
                        }
                    }

                    let state = aggregate_state(
                        &present.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &missing.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    );

                    results.push(PluginScopeStatus {
                        host: target.as_str().to_string(),
                        scope: scope.as_str().to_string(),
                        state,
                        present,
                        missing,
                        config_path: codex_dir.to_string_lossy().to_string(),
                    });
                }

                BridgeTarget::Kimetsu => {
                    // Not a user-installable host; skip.
                }

                BridgeTarget::Cursor => {
                    let cursor_dir = match home {
                        Some(h) => h.join(".cursor"),
                        None => workspace.join(".cursor"),
                    };

                    let mcp_ok = detect_cursor_mcp(&cursor_dir);
                    // Rules are workspace-only; only check when scope == Workspace.
                    let rules_ok = if home.is_none() {
                        detect_cursor_rules(&workspace)
                    } else {
                        true // global install doesn't write rules — not missing
                    };

                    let mut present = Vec::new();
                    let mut missing = Vec::new();

                    if mcp_ok {
                        present.push("mcp".to_string());
                    } else {
                        missing.push("mcp".to_string());
                    }
                    if !rules_ok {
                        missing.push("rules".to_string());
                    } else if home.is_none() {
                        present.push("rules".to_string());
                    }

                    let state = aggregate_state(
                        &present.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &missing.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    );

                    results.push(PluginScopeStatus {
                        host: target.as_str().to_string(),
                        scope: scope.as_str().to_string(),
                        state,
                        present,
                        missing,
                        config_path: cursor_dir.to_string_lossy().to_string(),
                    });
                }

                BridgeTarget::GeminiCli => {
                    let gemini_dir = match home {
                        Some(h) => h.join(".gemini"),
                        None => workspace.join(".gemini"),
                    };

                    let mcp_ok = detect_gemini_mcp(&gemini_dir);
                    let gemini_md_ok = if home.is_none() {
                        detect_gemini_md(&workspace)
                    } else {
                        detect_gemini_global_md(&gemini_dir)
                    };

                    let mut present = Vec::new();
                    let mut missing = Vec::new();

                    for (name, ok) in [("mcp", mcp_ok), ("GEMINI.md", gemini_md_ok)] {
                        if ok {
                            present.push(name.to_string());
                        } else {
                            missing.push(name.to_string());
                        }
                    }

                    let state = aggregate_state(
                        &present.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &missing.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    );

                    results.push(PluginScopeStatus {
                        host: target.as_str().to_string(),
                        scope: scope.as_str().to_string(),
                        state,
                        present,
                        missing,
                        config_path: gemini_dir.to_string_lossy().to_string(),
                    });
                }

                #[cfg(feature = "openclaw")]
                BridgeTarget::OpenClaw => {
                    // OpenClaw: global → ~/.openclaw/; workspace → .openclaw/
                    let oc_dir = match home {
                        Some(h) => h.join(".openclaw"),
                        None => workspace.join(".openclaw"),
                    };

                    let mcp_ok = detect_openclaw_mcp(&oc_dir);
                    let plugin_ok = detect_openclaw_plugin(&oc_dir);
                    let skill_ok = detect_openclaw_skill(&oc_dir);

                    let mut present = Vec::new();
                    let mut missing = Vec::new();

                    for (name, ok) in [("mcp", mcp_ok), ("plugin", plugin_ok), ("skill", skill_ok)]
                    {
                        if ok {
                            present.push(name.to_string());
                        } else {
                            missing.push(name.to_string());
                        }
                    }

                    let state = aggregate_state(
                        &present.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &missing.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    );

                    results.push(PluginScopeStatus {
                        host: target.as_str().to_string(),
                        scope: scope.as_str().to_string(),
                        state,
                        present,
                        missing,
                        config_path: oc_dir.to_string_lossy().to_string(),
                    });
                }

                #[cfg(feature = "pi")]
                BridgeTarget::Pi => {
                    // Pi: global → ~/.pi/agent/; workspace → .pi/
                    let pi_dir = match home {
                        Some(h) => h.join(".pi").join("agent"),
                        None => workspace.join(".pi"),
                    };

                    let ext_ok = detect_pi_extension(&pi_dir);
                    let skill_ok = detect_pi_skill(&pi_dir);

                    let mut present = Vec::new();
                    let mut missing = Vec::new();

                    for (name, ok) in [("extension", ext_ok), ("skill", skill_ok)] {
                        if ok {
                            present.push(name.to_string());
                        } else {
                            missing.push(name.to_string());
                        }
                    }

                    let state = aggregate_state(
                        &present.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                        &missing.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    );

                    results.push(PluginScopeStatus {
                        host: target.as_str().to_string(),
                        scope: scope.as_str().to_string(),
                        state,
                        present,
                        missing,
                        config_path: pi_dir.to_string_lossy().to_string(),
                    });
                }
            }
        }
    }

    results
}

/// Public entry point: read-only scan of Kimetsu plugin wiring.
pub fn plugin_status(workspace: &Path) -> Vec<PluginScopeStatus> {
    plugin_status_inner(workspace)
}

// ---------------------------------------------------------------------------
// plugin_uninstall — surgical inverse of plugin_install
// ---------------------------------------------------------------------------

/// What `plugin_uninstall` deleted or modified during its run.
#[derive(Debug, Clone, Default)]
pub struct PluginUninstallReport {
    /// Files / directories that were deleted entirely.
    pub removed: Vec<PathBuf>,
    /// Files whose content was edited (Kimetsu entries stripped).
    pub modified: Vec<PathBuf>,
}

pub fn plugin_uninstall(
    workspace: &Path,
    target: BridgeTarget,
    scope: InstallScope,
) -> Result<PluginUninstallReport, String> {
    let home = match scope {
        InstallScope::Global => Some(resolve_home()?),
        InstallScope::Workspace => None,
    };
    plugin_uninstall_inner(workspace, target, scope, home.as_deref())
}

/// `home` is `Some` for a global uninstall (the directory that stands in for
/// `~`), `None` for a workspace uninstall. Kept separate so tests can inject
/// a deterministic home, mirroring `plugin_install_inner`.
fn plugin_uninstall_inner(
    workspace: &Path,
    target: BridgeTarget,
    _scope: InstallScope,
    home: Option<&Path>,
) -> Result<PluginUninstallReport, String> {
    let workspace = normalize_path(workspace);
    let mut report = PluginUninstallReport::default();

    match target {
        BridgeTarget::ClaudeCode => {
            // MCP config: home → ~/.claude.json (mcpServers only);
            //             workspace → .mcp.json (servers + mcpServers).
            let (mcp_path, only_mcp_servers) = match home {
                Some(h) => (h.join(".claude.json"), true),
                None => (workspace.join(".mcp.json"), false),
            };
            if uninstall_mcp_config(&mcp_path, only_mcp_servers)? {
                report.modified.push(normalize_path(&mcp_path));
            }

            let claude_dir = match home {
                Some(h) => h.join(".claude"),
                None => workspace.join(".claude"),
            };

            // settings.json — strip Kimetsu hook groups.
            let settings = claude_dir.join("settings.json");
            if uninstall_claude_hooks(&settings)? {
                report.modified.push(normalize_path(&settings));
            }

            // CLAUDE.md — remove the <!-- kimetsu:begin/end --> block.
            let claude_md = claude_dir.join("CLAUDE.md");
            if uninstall_claude_md(&claude_md)? {
                report.modified.push(normalize_path(&claude_md));
            }

            // Delete commands/kimetsu/ directory.
            let commands_kimetsu = claude_dir.join("commands").join("kimetsu");
            if remove_path_if_exists(&commands_kimetsu)? {
                report.removed.push(normalize_path(&commands_kimetsu));
            }

            // Delete agents/kimetsu-memory-harvester.md.
            let harvester = claude_dir
                .join("agents")
                .join("kimetsu-memory-harvester.md");
            if remove_path_if_exists(&harvester)? {
                report.removed.push(normalize_path(&harvester));
            }
        }

        BridgeTarget::Codex => {
            let codex_dir = match home {
                Some(h) => h.join(".codex"),
                None => workspace.join(".codex"),
            };

            // config.toml — remove [mcp_servers.kimetsu].
            let config = codex_dir.join("config.toml");
            if uninstall_codex_config(&config)? {
                report.modified.push(normalize_path(&config));
            }

            // hooks.json — strip Kimetsu hook groups (same shape as Claude's).
            let hooks = codex_dir.join("hooks.json");
            if uninstall_codex_hooks(&hooks)? {
                report.modified.push(normalize_path(&hooks));
            }

            // Delete skills/kimetsu-bridge/ directory.
            let skill_dir = codex_dir.join("skills").join("kimetsu-bridge");
            if remove_path_if_exists(&skill_dir)? {
                report.removed.push(normalize_path(&skill_dir));
            }

            // Delete agents/kimetsu-memory-harvester.toml.
            let harvester = codex_dir
                .join("agents")
                .join("kimetsu-memory-harvester.toml");
            if remove_path_if_exists(&harvester)? {
                report.removed.push(normalize_path(&harvester));
            }
        }

        BridgeTarget::Kimetsu => {
            // Extensions are user data; uninstall is a no-op for this target.
        }

        BridgeTarget::Cursor => {
            let cursor_dir = match home {
                Some(h) => h.join(".cursor"),
                None => workspace.join(".cursor"),
            };

            // mcp.json — remove mcpServers.kimetsu.
            let mcp = cursor_dir.join("mcp.json");
            if uninstall_cursor_mcp(&mcp)? {
                report.modified.push(normalize_path(&mcp));
            }

            // Delete .cursor/rules/kimetsu-brain/ directory (workspace only).
            if home.is_none() {
                let rules_dir = workspace
                    .join(".cursor")
                    .join("rules")
                    .join("kimetsu-brain");
                if remove_path_if_exists(&rules_dir)? {
                    report.removed.push(normalize_path(&rules_dir));
                }
            }
        }

        BridgeTarget::GeminiCli => {
            let gemini_dir = match home {
                Some(h) => h.join(".gemini"),
                None => workspace.join(".gemini"),
            };

            // settings.json — remove mcpServers.kimetsu.
            let settings = gemini_dir.join("settings.json");
            if uninstall_gemini_settings(&settings)? {
                report.modified.push(normalize_path(&settings));
            }

            // GEMINI.md — remove the kimetsu block (workspace = project root;
            // global = ~/.gemini/GEMINI.md).
            let gemini_md = match home {
                Some(h) => h.join(".gemini").join("GEMINI.md"),
                None => workspace.join("GEMINI.md"),
            };
            if uninstall_gemini_md(&gemini_md)? {
                report.modified.push(normalize_path(&gemini_md));
            }
        }

        #[cfg(feature = "openclaw")]
        BridgeTarget::OpenClaw => {
            let oc_dir = match home {
                Some(h) => h.join(".openclaw"),
                None => workspace.join(".openclaw"),
            };

            // openclaw.json — strip mcp.servers.kimetsu + plugins.entries.kimetsu.
            let config = oc_dir.join("openclaw.json");
            if uninstall_openclaw_config(&config)? {
                report.modified.push(normalize_path(&config));
            }

            // Delete plugins/kimetsu/ directory.
            let plugin_dir = oc_dir.join("plugins").join("kimetsu");
            if remove_path_if_exists(&plugin_dir)? {
                report.removed.push(normalize_path(&plugin_dir));
            }

            // Delete workspace/skills/kimetsu-context/ directory.
            let skill_dir = oc_dir
                .join("workspace")
                .join("skills")
                .join("kimetsu-context");
            if remove_path_if_exists(&skill_dir)? {
                report.removed.push(normalize_path(&skill_dir));
            }
        }

        #[cfg(feature = "pi")]
        BridgeTarget::Pi => {
            let pi_dir = match home {
                Some(h) => h.join(".pi").join("agent"),
                None => workspace.join(".pi"),
            };

            // Delete extensions/kimetsu.ts
            let ext_file = pi_dir.join("extensions").join("kimetsu.ts");
            if remove_path_if_exists(&ext_file)? {
                report.removed.push(normalize_path(&ext_file));
            }

            // Strip kimetsu entry from settings.json
            let settings = pi_dir.join("settings.json");
            if uninstall_pi_settings(&settings)? {
                report.modified.push(normalize_path(&settings));
            }

            // Delete skills/kimetsu-brain/ directory
            let skill_dir = pi_dir.join("skills").join("kimetsu-brain");
            if remove_path_if_exists(&skill_dir)? {
                report.removed.push(normalize_path(&skill_dir));
            }
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Uninstall helpers
// ---------------------------------------------------------------------------

/// Remove `path` if it exists (file or directory). Returns `true` if something
/// was actually deleted. A missing path is not an error.
fn remove_path_if_exists(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|err| format!("remove dir {}: {err}", path.display()))?;
    } else {
        fs::remove_file(path).map_err(|err| format!("remove file {}: {err}", path.display()))?;
    }
    Ok(true)
}

/// Strip Kimetsu hook groups from `settings.json`. Returns `true` if the file
/// was changed and written back. Missing file → Ok(false).
fn uninstall_claude_hooks(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(strip_bom(&text))
        .map_err(|err| format!("parse {}: {err}", path.display()))?;

    let Some(root_obj) = root.as_object_mut() else {
        return Ok(false);
    };

    let Some(hooks_value) = root_obj.get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_obj) = hooks_value.as_object_mut() else {
        return Ok(false);
    };

    // For each event array, retain only non-Kimetsu groups.
    let mut events_to_remove: Vec<String> = Vec::new();
    let mut changed = false;
    for (event, groups_value) in hooks_obj.iter_mut() {
        let Some(groups) = groups_value.as_array_mut() else {
            continue;
        };
        let before = groups.len();
        groups.retain(|g| !is_kimetsu_hook_group(g));
        if groups.len() != before {
            changed = true;
        }
        if groups.is_empty() {
            events_to_remove.push(event.clone());
        }
    }
    for event in events_to_remove {
        hooks_obj.remove(&event);
    }

    // If `hooks` itself became empty, remove it.
    if hooks_obj.is_empty() {
        root_obj.remove("hooks");
    }

    if !changed {
        return Ok(false);
    }

    let out = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
}

/// Strip Kimetsu hook groups from `.codex/hooks.json`. The JSON shape used by
/// Codex is `{ "hooks": { "<Event>": [ <group>, … ] } }` — identical to
/// Claude's `settings.json`, so we reuse the same removal logic.
fn uninstall_codex_hooks(path: &Path) -> Result<bool, String> {
    // Codex hooks.json uses the same `{ "hooks": { … } }` structure as
    // Claude's settings.json, so the same function applies.
    uninstall_claude_hooks(path)
}

/// Remove the `"kimetsu"` key from `mcpServers` (and optionally `servers`) in
/// the given JSON config file. Returns `true` if the file was changed.
fn uninstall_mcp_config(path: &Path, only_mcp_servers: bool) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(strip_bom(&text))
        .map_err(|err| format!("parse {}: {err}", path.display()))?;

    let Some(root_obj) = root.as_object_mut() else {
        return Ok(false);
    };

    let mut changed = false;

    if !only_mcp_servers {
        if let Some(servers) = root_obj.get_mut("servers").and_then(|v| v.as_object_mut()) {
            if servers.remove("kimetsu").is_some() {
                changed = true;
            }
        }
    }
    if let Some(mcp_servers) = root_obj
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
    {
        if mcp_servers.remove("kimetsu").is_some() {
            changed = true;
        }
    }

    if !changed {
        return Ok(false);
    }

    let out = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
}

/// Remove the `<!-- kimetsu:begin --> … <!-- kimetsu:end -->` block from
/// `CLAUDE.md`. Returns `true` if the file was changed.
fn uninstall_claude_md(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let raw = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let text = strip_bom(&raw);

    let (begin_pos, end_pos) = match (text.find(CLAUDE_MD_BEGIN), text.find(CLAUDE_MD_END)) {
        (Some(b), Some(e)) if e >= b => (b, e),
        _ => return Ok(false), // block absent or malformed — nothing to remove
    };

    let end_of_block = end_pos + CLAUDE_MD_END.len();
    // Consume one trailing newline if present (the block is written with one).
    let after_start = if text[end_of_block..].starts_with('\n') {
        end_of_block + 1
    } else {
        end_of_block
    };

    let before = &text[..begin_pos];
    let after = &text[after_start..];

    // Trim the trailing separator blank line that merge_claude_md left before
    // the block (the "\n\n" before CLAUDE_MD_BEGIN), so we don't leave a
    // doubled blank line where the block used to be.
    let before_trimmed = before.trim_end_matches('\n');
    let merged = if before_trimmed.is_empty() {
        // The Kimetsu block was the entire file (or at the very start).
        after.to_string()
    } else if after.is_empty() || after.trim().is_empty() {
        // Nothing after the block — just the user's content.
        format!("{before_trimmed}\n")
    } else {
        // User content before and after — rejoin with a single blank line.
        format!("{before_trimmed}\n\n{after}")
    };

    write_text_file(path, &merged, true)?;
    Ok(true)
}

/// Remove the `[mcp_servers.kimetsu]` entry from `.codex/config.toml`.
/// Returns `true` if the file was changed.
fn uninstall_codex_config(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: toml::Value = toml::from_str(strip_bom(&text))
        .map_err(|err| format!("parse {}: {err}", path.display()))?;

    let Some(root_table) = root.as_table_mut() else {
        return Ok(false);
    };
    let Some(servers_value) = root_table.get_mut("mcp_servers") else {
        return Ok(false);
    };
    let Some(servers) = servers_value.as_table_mut() else {
        return Ok(false);
    };

    if servers.remove("kimetsu").is_none() {
        return Ok(false);
    }

    let out = toml::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
}

/// Upsert `mcpServers.kimetsu` into Cursor's `mcp.json`.
///
/// Schema verified from https://cursor.com/docs/mcp (June 2026):
/// - STDIO server uses `type: "stdio"`, `command`, and `args` fields.
/// - Both workspace (`.cursor/mcp.json`) and global (`~/.cursor/mcp.json`)
///   use the same `mcpServers` key — only `mcpServers`, no `servers` twin key.
fn write_cursor_mcp_config(path: &Path) -> Result<(), String> {
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
    let servers = root_obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| format!("{} `mcpServers` must be a JSON object", path.display()))?;
    servers_obj.insert(
        "kimetsu".to_string(),
        serde_json::json!({
            "type": "stdio",
            "command": "kimetsu",
            "args": ["mcp", "serve", "--workspace", "."]
        }),
    );
    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &text, true)
}

/// Remove `mcpServers.kimetsu` from Cursor's `mcp.json`.
/// Returns `true` if the file was changed.
fn uninstall_cursor_mcp(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(strip_bom(&text))
        .map_err(|err| format!("parse {}: {err}", path.display()))?;
    let Some(root_obj) = root.as_object_mut() else {
        return Ok(false);
    };
    let Some(servers) = root_obj
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
    else {
        return Ok(false);
    };
    if servers.remove("kimetsu").is_none() {
        return Ok(false);
    }
    let out = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
}

/// Upsert `mcpServers.kimetsu` into Gemini CLI's `settings.json`.
///
/// Schema verified from google-gemini/gemini-cli docs (June 2026):
/// - STDIO server uses `command` + `args` fields under the `mcpServers` key.
/// - Both workspace (`.gemini/settings.json`) and global (`~/.gemini/settings.json`)
///   use `mcpServers` — same as Gemini's own documented examples.
fn write_gemini_settings(path: &Path) -> Result<(), String> {
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
    let servers = root_obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| format!("{} `mcpServers` must be a JSON object", path.display()))?;
    servers_obj.insert(
        "kimetsu".to_string(),
        serde_json::json!({
            "command": "kimetsu",
            "args": ["mcp", "serve", "--workspace", "."]
        }),
    );
    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &text, true)
}

/// Remove `mcpServers.kimetsu` from Gemini CLI's `settings.json`.
/// Returns `true` if the file was changed.
fn uninstall_gemini_settings(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(strip_bom(&text))
        .map_err(|err| format!("parse {}: {err}", path.display()))?;
    let Some(root_obj) = root.as_object_mut() else {
        return Ok(false);
    };
    let Some(servers) = root_obj
        .get_mut("mcpServers")
        .and_then(|v| v.as_object_mut())
    else {
        return Ok(false);
    };
    if servers.remove("kimetsu").is_none() {
        return Ok(false);
    }
    let out = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
}

/// Merge the Kimetsu brain guidance block into a `GEMINI.md` file.
///
/// Uses the same `<!-- kimetsu:begin/end -->` marker idiom as `merge_claude_md`
/// so the block can be found and updated idempotently. Missing file → create.
/// Existing user content is never clobbered.
fn merge_gemini_md(path: &Path) -> Result<(), String> {
    let block = format!("{GEMINI_MD_BEGIN}\n{GEMINI_MD_CONTENT}{GEMINI_MD_END}\n");
    let raw = if path.is_file() {
        fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?
    } else {
        String::new()
    };
    let existing = strip_bom(&raw);
    let merged = match (existing.find(GEMINI_MD_BEGIN), existing.find(GEMINI_MD_END)) {
        (Some(start), Some(end_start)) if end_start >= start => {
            let end = end_start + GEMINI_MD_END.len();
            let after = existing[end..]
                .strip_prefix('\n')
                .unwrap_or(&existing[end..]);
            format!("{}{block}{after}", &existing[..start])
        }
        (Some(start), _) => {
            // BEGIN present but END missing — corrupt; replace from BEGIN onward.
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
                out.push('\n');
            }
            out.push_str(&block);
            out
        }
    };
    write_text_file(path, &merged, true)
}

/// Remove the `<!-- kimetsu:begin --> … <!-- kimetsu:end -->` block from
/// `GEMINI.md`. Returns `true` if the file was changed.
fn uninstall_gemini_md(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let raw = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let text = strip_bom(&raw);

    let (begin_pos, end_pos) = match (text.find(GEMINI_MD_BEGIN), text.find(GEMINI_MD_END)) {
        (Some(b), Some(e)) if e >= b => (b, e),
        _ => return Ok(false),
    };

    let end_of_block = end_pos + GEMINI_MD_END.len();
    let after_start = if text[end_of_block..].starts_with('\n') {
        end_of_block + 1
    } else {
        end_of_block
    };

    let before = &text[..begin_pos];
    let after = &text[after_start..];
    let before_trimmed = before.trim_end_matches('\n');
    let merged = if before_trimmed.is_empty() {
        after.to_string()
    } else if after.is_empty() || after.trim().is_empty() {
        format!("{before_trimmed}\n")
    } else {
        format!("{before_trimmed}\n\n{after}")
    };

    write_text_file(path, &merged, true)?;
    Ok(true)
}

#[cfg(feature = "pi")]
/// Strip the `"./extensions/kimetsu.ts"` entry from Pi's `settings.json`.
/// Returns `true` if the file was changed. Missing file or absent entry → Ok(false).
fn uninstall_pi_settings(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(strip_bom(&text))
        .map_err(|err| format!("parse {}: {err}", path.display()))?;

    let Some(root_obj) = root.as_object_mut() else {
        return Ok(false);
    };
    let Some(extensions_value) = root_obj.get_mut("extensions") else {
        return Ok(false);
    };
    let Some(arr) = extensions_value.as_array_mut() else {
        return Ok(false);
    };

    const EXT_PATH: &str = "./extensions/kimetsu.ts";
    let before = arr.len();
    arr.retain(|v| v.as_str() != Some(EXT_PATH));

    if arr.len() == before {
        return Ok(false); // nothing removed
    }

    // If the extensions array is now empty, remove it entirely.
    if arr.is_empty() {
        root_obj.remove("extensions");
    }

    let out = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
}

#[cfg(feature = "openclaw")]
/// Upsert the `kimetsu` MCP server and plugin entry into `openclaw.json`.
///
/// `openclaw.json` is JSON5 (supports comments and trailing commas). We parse
/// it with `json5` to tolerate the source format, then write it back with
/// `serde_json::to_string_pretty`. **Comments in the original file are lost**
/// after the first Kimetsu install — we push a note so the caller can surface
/// this to the user. Idempotent: re-running just refreshes the same entries,
/// preserving all other keys.
fn write_openclaw_config(path: &Path, notes: &mut Vec<String>) -> Result<(), String> {
    let had_file = path.is_file();
    let mut root = if had_file {
        let text =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
        json5::from_str::<serde_json::Value>(&text)
            .map_err(|err| format!("parse {}: {err}", path.display()))?
    } else {
        serde_json::json!({})
    };

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} must be a JSON object", path.display()))?;

    // Upsert mcp.servers.kimetsu
    {
        let mcp = root_obj
            .entry("mcp".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let mcp_obj = mcp
            .as_object_mut()
            .ok_or_else(|| format!("{} `mcp` must be a JSON object", path.display()))?;
        let servers = mcp_obj
            .entry("servers".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let servers_obj = servers
            .as_object_mut()
            .ok_or_else(|| format!("{} `mcp.servers` must be a JSON object", path.display()))?;
        servers_obj.insert(
            "kimetsu".to_string(),
            serde_json::json!({
                "command": "kimetsu",
                "args": ["mcp", "serve", "--workspace", "."]
            }),
        );
    }

    // Upsert plugins.entries.kimetsu (activation config)
    {
        let plugins = root_obj
            .entry("plugins".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let plugins_obj = plugins
            .as_object_mut()
            .ok_or_else(|| format!("{} `plugins` must be a JSON object", path.display()))?;
        let entries = plugins_obj
            .entry("entries".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let entries_obj = entries
            .as_object_mut()
            .ok_or_else(|| format!("{} `plugins.entries` must be a JSON object", path.display()))?;
        entries_obj.insert(
            "kimetsu".to_string(),
            serde_json::json!({
                "hooks": {
                    "timeoutMs": 30000,
                    "allowConversationAccess": false
                }
            }),
        );
    }

    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &text, true)?;

    // Warn that JSON5 source comments are not preserved after the rewrite.
    if had_file {
        notes.push(format!(
            "note: {} was reformatted as JSON; comments not preserved",
            path.display()
        ));
    }

    Ok(())
}

#[cfg(feature = "openclaw")]
/// Strip `mcp.servers.kimetsu` and `plugins.entries.kimetsu` from `openclaw.json`.
///
/// Reads the file as JSON5 (tolerating comments), removes Kimetsu's entries,
/// and writes back as plain JSON. Preserves all other keys. Returns `true` if
/// the file was modified.
fn uninstall_openclaw_config(path: &Path) -> Result<bool, String> {
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut root: serde_json::Value =
        json5::from_str(&text).map_err(|err| format!("parse {}: {err}", path.display()))?;

    let Some(root_obj) = root.as_object_mut() else {
        return Ok(false);
    };

    let mut changed = false;

    // Remove mcp.servers.kimetsu
    if let Some(mcp) = root_obj.get_mut("mcp").and_then(|v| v.as_object_mut()) {
        if let Some(servers) = mcp.get_mut("servers").and_then(|v| v.as_object_mut()) {
            if servers.remove("kimetsu").is_some() {
                changed = true;
            }
        }
    }

    // Remove plugins.entries.kimetsu
    if let Some(plugins) = root_obj.get_mut("plugins").and_then(|v| v.as_object_mut()) {
        if let Some(entries) = plugins.get_mut("entries").and_then(|v| v.as_object_mut()) {
            if entries.remove("kimetsu").is_some() {
                changed = true;
            }
        }
    }

    if !changed {
        return Ok(false);
    }

    let out = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize {}: {err}", path.display()))?;
    write_text_file(path, &out, true)?;
    Ok(true)
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

    // Codex has no session-start event; the daemon warms lazily on the first prompt instead.

    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize Codex hooks: {err}"))?;
    write_text_file(&hooks, &text, true)?;
    files.push(normalize_path(&hooks));
    Ok(())
}

#[cfg(feature = "pi")]
/// Idempotently register Kimetsu's TS extension in Pi's `settings.json`.
///
/// Pi discovers extensions from the `"extensions"` array of absolute paths.
/// We append `"./extensions/kimetsu.ts"` (relative) if not already present,
/// preserving all other keys. Missing file → creates `{ "extensions": ["./extensions/kimetsu.ts"] }`.
fn write_pi_settings(path: &Path) -> Result<(), String> {
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

    let extensions = root_obj
        .entry("extensions".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));

    let arr = extensions
        .as_array_mut()
        .ok_or_else(|| format!("{} `extensions` must be an array", path.display()))?;

    const EXT_PATH: &str = "./extensions/kimetsu.ts";

    // Idempotent: only add if not already present.
    let already_registered = arr.iter().any(|v| v.as_str() == Some(EXT_PATH));

    if !already_registered {
        arr.push(serde_json::Value::String(EXT_PATH.to_string()));
    }

    let text = serde_json::to_string_pretty(&root)
        .map_err(|err| format!("serialize Pi settings: {err}"))?;
    write_text_file(path, &text, true)
}

fn import_skill_manifest(
    workspace: &Path,
    skill: &SkillManifest,
    force: bool,
) -> Result<BridgeExtension, String> {
    let id = slugify(&skill.name);
    let destination = extensions_root(workspace).join(&id);
    copy_dir_with_replace(
        &skill.root,
        &destination,
        &extensions_root(workspace),
        force,
    )?;
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
    let server = serde_json::json!({
        "command": "kimetsu",
        "args": ["mcp", "serve", "--workspace", "."]
    });
    write_mcp_config_server(path, only_mcp_servers, server)
}

/// Upsert the `kimetsu` MCP server entry with an arbitrary server value (stdio
/// `command`/`args`, or a remote `type`/`url`/`headers` object).
fn write_mcp_config_server(
    path: &Path,
    only_mcp_servers: bool,
    server: serde_json::Value,
) -> Result<(), String> {
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
///
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
    upsert_kimetsu_hook(
        hooks_obj,
        "SessionStart",
        serde_json::json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": "kimetsu brain warm" }]
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

fn copy_dir_with_replace(
    source: &Path,
    destination: &Path,
    allowed_root: &Path,
    force: bool,
) -> Result<(), String> {
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
        remove_dir_checked(destination, allowed_root)?;
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

fn remove_dir_checked(path: &Path, allowed_root: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|err| format!("inspect {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing to remove symlink {}", path.display()));
    }
    let target = path
        .canonicalize()
        .map_err(|err| format!("resolve {}: {err}", path.display()))?;
    let allowed_root = allowed_root
        .canonicalize()
        .map_err(|err| format!("resolve {}: {err}", allowed_root.display()))?;
    if !target.starts_with(&allowed_root) {
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
    fn remote_install_claude_writes_http_mcp_entry() {
        let root = temp_root("remote_claude");

        // No token → env-var reference; trailing slash on base trimmed.
        plugin_install_remote(
            &root,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            &RemoteInstall {
                base_url: "https://kimetsu.example.com:8787/".to_string(),
                repo_id: "demo-repo".to_string(),
                token: None,
            },
        )
        .expect("remote install");

        let text = fs::read_to_string(root.join(".mcp.json")).expect("read .mcp.json");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let server = &v["mcpServers"]["kimetsu"];
        assert_eq!(server["type"], "http");
        assert_eq!(
            server["url"],
            "https://kimetsu.example.com:8787/mcp/demo-repo"
        );
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${KIMETSU_REMOTE_TOKEN}"
        );
        // No local stdio command must be written for a remote entry.
        assert!(server.get("command").is_none());

        // status sees the mcp piece.
        let statuses = plugin_status_inner(&root);
        let ws = statuses
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("claude-code/workspace");
        assert!(ws.present.contains(&"mcp".to_string()));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn remote_install_literal_token_is_written() {
        let root = temp_root("remote_token");
        plugin_install_remote(
            &root,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            &RemoteInstall {
                base_url: "http://localhost:8787".to_string(),
                repo_id: "r".to_string(),
                token: Some("tok_secret".to_string()),
            },
        )
        .expect("remote install");
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["kimetsu"]["headers"]["Authorization"],
            "Bearer tok_secret"
        );
        fs::remove_dir_all(root).ok();
    }

    #[cfg(feature = "openclaw")]
    #[test]
    fn remote_install_openclaw_writes_url_transport() {
        let root = temp_root("remote_openclaw");
        plugin_install_remote(
            &root,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            &RemoteInstall {
                base_url: "https://h:8787".to_string(),
                repo_id: "demo".to_string(),
                token: None,
            },
        )
        .expect("remote install");
        let v: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join(".openclaw").join("openclaw.json")).unwrap(),
        )
        .unwrap();
        let server = &v["mcp"]["servers"]["kimetsu"];
        assert_eq!(server["url"], "https://h:8787/mcp/demo");
        assert_eq!(server["transport"], "streamable-http");
        assert!(server.get("command").is_none());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn remote_install_rejects_unsupported_host() {
        let root = temp_root("remote_codex");
        let err = plugin_install_remote(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            &RemoteInstall {
                base_url: "http://h".to_string(),
                repo_id: "r".to_string(),
                token: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("remote install is supported"));
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
        fs::write(&p, "\u{feff}# My rules\n").unwrap();
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

    #[test]
    fn copy_dir_with_replace_refuses_symlink_destination() {
        let root = temp_root("bridge_symlink_dest");
        let source = root.join("source");
        let allowed = root.join("allowed");
        let outside = root.join("outside");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("SKILL.md"), "safe").expect("skill");
        fs::create_dir_all(&allowed).expect("allowed");
        fs::create_dir_all(&outside).expect("outside");

        let destination = allowed.join("skill");
        if create_dir_symlink(&outside, &destination).is_err() {
            fs::remove_dir_all(root).ok();
            return;
        }

        let err = copy_dir_with_replace(&source, &destination, &allowed, true)
            .expect_err("symlink destination must be rejected");
        assert!(err.contains("refusing to remove symlink"), "got: {err}");
        assert!(outside.exists(), "linked outside directory must survive");

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

    #[cfg(unix)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_dir_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(target, link)
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

    // -------------------------------------------------------------------------
    // U1 — plugin_uninstall tests (TDD golden tests)
    // -------------------------------------------------------------------------

    /// Round-trip: install then uninstall leaves user content untouched and
    /// removes every Kimetsu artefact (Claude Code, workspace scope).
    #[test]
    fn u1_roundtrip_claude_workspace() {
        let ws = temp_root("u1_roundtrip_cc_ws");
        let claude_dir = ws.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Pre-seed user content in every file that install merges into.
        fs::write(
            claude_dir.join("CLAUDE.md"),
            "# My workspace rules\nAlways write tests.\n",
        )
        .unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "myPref": true,
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-pretool" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            ws.join(".mcp.json"),
            serde_json::to_string_pretty(&json!({
                "servers": { "my-server": { "command": "my-cmd" } },
                "mcpServers": { "my-server": { "command": "my-cmd" } }
            }))
            .unwrap(),
        )
        .unwrap();

        // Install.
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

        // Verify install left its artefacts.
        assert!(claude_dir.join("commands/kimetsu/bridge.md").is_file());
        assert!(
            claude_dir
                .join("agents/kimetsu-memory-harvester.md")
                .is_file()
        );

        // Uninstall.
        let report =
            plugin_uninstall_inner(&ws, BridgeTarget::ClaudeCode, InstallScope::Workspace, None)
                .unwrap();

        // Kimetsu artefact files are gone.
        assert!(
            !claude_dir.join("commands/kimetsu").exists(),
            "commands/kimetsu dir must be removed"
        );
        assert!(
            !claude_dir
                .join("agents/kimetsu-memory-harvester.md")
                .exists(),
            "harvester agent must be removed"
        );
        assert!(
            report
                .removed
                .iter()
                .any(|p| p.ends_with("kimetsu") || p.to_string_lossy().contains("kimetsu")),
            "report.removed must mention kimetsu"
        );

        // User CLAUDE.md content survived; Kimetsu block is gone.
        let md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert!(md.contains("# My workspace rules"), "user CLAUDE.md kept");
        assert!(md.contains("Always write tests."), "user detail kept");
        assert!(
            !md.contains(CLAUDE_MD_BEGIN),
            "kimetsu begin marker must be removed"
        );
        assert!(
            !md.contains(CLAUDE_MD_END),
            "kimetsu end marker must be removed"
        );

        // User hook survived; Kimetsu hooks are gone.
        let sv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
                .unwrap();
        let pre = sv["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            pre.iter()
                .any(|g| g["hooks"][0]["command"] == "user-pretool"),
            "user PreToolUse hook must survive"
        );
        assert!(
            !pre.iter().any(is_kimetsu_hook_group),
            "no Kimetsu hook groups must remain"
        );
        assert_eq!(sv["myPref"], true, "top-level pref must survive");
        // Kimetsu-only events should be gone.
        assert!(
            sv["hooks"].get("Stop").is_none() || {
                sv["hooks"]["Stop"]
                    .as_array()
                    .map(|a| a.iter().all(|g| !is_kimetsu_hook_group(g)))
                    .unwrap_or(true)
            },
            "no Kimetsu Stop hook groups must remain"
        );

        // User MCP server survived; Kimetsu key removed.
        let mv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ws.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(
            mv["servers"]["my-server"]["command"], "my-cmd",
            "user server kept"
        );
        assert!(
            mv["servers"].get("kimetsu").is_none(),
            "kimetsu servers entry removed"
        );
        assert_eq!(mv["mcpServers"]["my-server"]["command"], "my-cmd");
        assert!(
            mv["mcpServers"].get("kimetsu").is_none(),
            "kimetsu mcpServers entry removed"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Idempotent on a clean host: uninstall on a workspace with no Kimetsu
    /// wiring must succeed and remove nothing.
    #[test]
    fn u1_idempotent_on_clean_host() {
        let ws = temp_root("u1_clean_host");
        // No files at all — completely empty workspace.
        let report =
            plugin_uninstall_inner(&ws, BridgeTarget::ClaudeCode, InstallScope::Workspace, None)
                .unwrap();
        assert!(
            report.removed.is_empty(),
            "nothing removed on a clean host (Claude)"
        );
        assert!(
            report.modified.is_empty(),
            "nothing modified on a clean host (Claude)"
        );

        let report2 =
            plugin_uninstall_inner(&ws, BridgeTarget::Codex, InstallScope::Workspace, None)
                .unwrap();
        assert!(
            report2.removed.is_empty(),
            "nothing removed on a clean host (Codex)"
        );
        assert!(
            report2.modified.is_empty(),
            "nothing modified on a clean host (Codex)"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Codex round-trip: install then uninstall leaves user codex content intact
    /// and removes Kimetsu's config.toml entry, hooks.json groups, skill dir, and
    /// agent file.
    #[test]
    fn u1_roundtrip_codex_workspace() {
        let ws = temp_root("u1_roundtrip_codex_ws");
        let codex_dir = ws.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();

        // Pre-seed a user hook on the UserPromptSubmit event.
        fs::write(
            codex_dir.join("hooks.json"),
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "UserPromptSubmit": [
                        { "matcher": "", "hooks": [{ "type": "command", "command": "user-codex-hook" }] }
                    ],
                    "SubagentStop": [
                        { "matcher": "", "hooks": [{ "type": "command", "command": "user-subagent-hook" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // Pre-seed a user MCP server in config.toml.
        fs::write(
            codex_dir.join("config.toml"),
            "[mcp_servers.my-server]\ncommand = \"my-server-cmd\"\nargs = []\n",
        )
        .unwrap();

        // Install.
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

        // Verify install wrote its artefacts.
        assert!(codex_dir.join("skills/kimetsu-bridge/SKILL.md").is_file());
        assert!(
            codex_dir
                .join("agents/kimetsu-memory-harvester.toml")
                .is_file()
        );

        // Uninstall.
        let report =
            plugin_uninstall_inner(&ws, BridgeTarget::Codex, InstallScope::Workspace, None)
                .unwrap();

        // Kimetsu artefacts gone.
        assert!(
            !codex_dir.join("skills/kimetsu-bridge").exists(),
            "kimetsu-bridge skill dir must be removed"
        );
        assert!(
            !codex_dir
                .join("agents/kimetsu-memory-harvester.toml")
                .exists(),
            "harvester toml must be removed"
        );
        assert!(
            !report.removed.is_empty(),
            "report.removed must be non-empty"
        );

        // config.toml: user server survives, kimetsu entry gone.
        let config_text = fs::read_to_string(codex_dir.join("config.toml")).unwrap();
        let config_val: toml::Value = toml::from_str(&config_text).unwrap();
        assert!(
            config_val["mcp_servers"].get("my-server").is_some(),
            "user mcp server must survive in config.toml"
        );
        assert!(
            config_val["mcp_servers"].get("kimetsu").is_none(),
            "kimetsu mcp server must be removed from config.toml"
        );

        // hooks.json: user hooks survive, Kimetsu groups gone.
        let hooks_val: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(codex_dir.join("hooks.json")).unwrap())
                .unwrap();
        let ups = hooks_val["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert!(
            ups.iter()
                .any(|g| g["hooks"][0]["command"] == "user-codex-hook"),
            "user UserPromptSubmit hook must survive"
        );
        assert!(
            !ups.iter().any(is_kimetsu_hook_group),
            "no Kimetsu UserPromptSubmit groups must remain"
        );
        // SubagentStop (user-only event) must survive untouched.
        assert_eq!(
            hooks_val["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
            "user-subagent-hook"
        );
        // Kimetsu-only Stop event should have no Kimetsu groups.
        if let Some(stop) = hooks_val["hooks"].get("Stop").and_then(|v| v.as_array()) {
            assert!(
                !stop.iter().any(is_kimetsu_hook_group),
                "no Kimetsu Stop groups must remain"
            );
        }

        fs::remove_dir_all(ws).ok();
    }

    /// Preserves a user hook on the SAME event Kimetsu uses (UserPromptSubmit).
    /// Install adds Kimetsu's group alongside the user's; uninstall removes
    /// Kimetsu's leaving exactly the user's group.
    #[test]
    fn u1_preserves_user_hook_on_shared_event() {
        let ws = temp_root("u1_shared_event");
        let claude_dir = ws.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Seed a user UserPromptSubmit hook.
        fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "UserPromptSubmit": [
                        { "matcher": "", "hooks": [{ "type": "command", "command": "user-ups-hook" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // Install (adds Kimetsu's UserPromptSubmit group alongside).
        plugin_install_inner(
            &ws,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false, // proactive=false: skip PreToolUse/PostToolUse
            None,
        )
        .unwrap();

        // Verify both groups exist after install.
        let sv: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
                .unwrap();
        let ups = sv["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(
            ups.len(),
            2,
            "both user and kimetsu groups present after install"
        );

        // Uninstall.
        plugin_uninstall_inner(&ws, BridgeTarget::ClaudeCode, InstallScope::Workspace, None)
            .unwrap();

        // Exactly one UserPromptSubmit group remains: the user's.
        let sv2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(claude_dir.join("settings.json")).unwrap())
                .unwrap();
        let ups2 = sv2["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(
            ups2.len(),
            1,
            "exactly one UserPromptSubmit group survives uninstall"
        );
        assert_eq!(
            ups2[0]["hooks"][0]["command"], "user-ups-hook",
            "the surviving group is the user's"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Global scope round-trip (Claude Code): install into injected home, uninstall
    /// from the same home — workspace must remain untouched throughout.
    #[test]
    fn u1_roundtrip_claude_global() {
        let ws = temp_root("u1_roundtrip_cc_global_ws");
        let home = temp_root("u1_roundtrip_cc_global_home");
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Pre-seed user content in global home.
        fs::write(
            claude_dir.join("CLAUDE.md"),
            "# Global rules\nUse conventional commits.\n",
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": { "user-global": { "command": "user-global-cmd" } }
            }))
            .unwrap(),
        )
        .unwrap();

        // Install (global).
        plugin_install_inner(
            &ws,
            BridgeTarget::ClaudeCode,
            InstallScope::Global,
            PluginMode::Optional,
            false,
            false,
            Some(home.as_path()),
        )
        .unwrap();

        assert!(home.join(".claude.json").is_file());
        assert!(claude_dir.join("commands/kimetsu/bridge.md").is_file());

        // Uninstall (global).
        plugin_uninstall_inner(
            &ws,
            BridgeTarget::ClaudeCode,
            InstallScope::Global,
            Some(home.as_path()),
        )
        .unwrap();

        // Workspace untouched.
        assert!(!ws.join(".claude").exists(), "workspace .claude untouched");
        assert!(
            !ws.join(".mcp.json").exists(),
            "workspace .mcp.json untouched"
        );

        // Kimetsu artefacts in home are gone.
        assert!(
            !claude_dir.join("commands/kimetsu").exists(),
            "commands/kimetsu removed from home"
        );
        assert!(
            !claude_dir
                .join("agents/kimetsu-memory-harvester.md")
                .exists(),
            "harvester removed from home"
        );

        // User CLAUDE.md content survived; block gone.
        let md = fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
        assert!(md.contains("# Global rules"), "user global CLAUDE.md kept");
        assert!(!md.contains(CLAUDE_MD_BEGIN), "kimetsu block removed");

        // User MCP server in ~/.claude.json survived; kimetsu key gone.
        let cj: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(
            cj["mcpServers"]["user-global"]["command"],
            "user-global-cmd"
        );
        assert!(
            cj["mcpServers"].get("kimetsu").is_none(),
            "kimetsu mcpServers entry removed"
        );

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    // -------------------------------------------------------------------------
    // QQ1 — plugin_status detection tests
    // -------------------------------------------------------------------------

    /// Fresh workspace (nothing installed) → all four scopes Absent.
    #[test]
    fn qq1_status_fresh_workspace_all_absent() {
        let root = temp_root("qq1_status_fresh");
        let statuses = plugin_status_inner(&root);
        // Should have 4 entries: ClaudeCode/workspace, ClaudeCode/global,
        // Codex/workspace, Codex/global (global may be absent if HOME works).
        assert!(!statuses.is_empty(), "should have status entries");
        for s in &statuses {
            // A fresh workspace has nothing; workspace-scope entries must be Absent.
            if s.scope == "workspace" {
                assert!(
                    matches!(s.state, WiringState::Absent),
                    "{}/{} should be Absent in fresh workspace, got present={:?}",
                    s.host,
                    s.scope,
                    s.present
                );
            }
        }
        fs::remove_dir_all(root).ok();
    }

    /// After install (ClaudeCode, Workspace) → that entry is Installed with
    /// correct present pieces; others stay Absent for workspace scope.
    #[test]
    fn qq1_status_after_claude_code_workspace_install() {
        let root = temp_root("qq1_status_after_install");
        let fake_home = temp_root("qq1_status_home");

        // Install with injected home so global detection uses fake_home.
        plugin_install_inner(
            &root,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true, // proactive
            None, // workspace scope → no home needed
        )
        .expect("install");

        let statuses = plugin_status_inner(&root);

        let ws_claude = statuses
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("claude-code/workspace entry");

        assert!(
            matches!(ws_claude.state, WiringState::Installed),
            "claude-code/workspace should be Installed after install; present={:?} missing={:?}",
            ws_claude.present,
            ws_claude.missing
        );
        assert!(
            ws_claude.present.contains(&"hooks".to_string()),
            "hooks should be present"
        );
        assert!(
            ws_claude.present.contains(&"mcp".to_string()),
            "mcp should be present"
        );
        assert!(
            ws_claude.present.contains(&"CLAUDE.md".to_string()),
            "CLAUDE.md should be present"
        );
        assert!(
            ws_claude.present.contains(&"commands".to_string()),
            "commands should be present"
        );
        assert!(
            ws_claude.present.contains(&"agent".to_string()),
            "agent should be present"
        );
        assert!(ws_claude.missing.is_empty(), "nothing should be missing");

        // Codex workspace should still be Absent.
        let ws_codex = statuses
            .iter()
            .find(|s| s.host == "codex" && s.scope == "workspace")
            .expect("codex/workspace entry");
        assert!(
            matches!(ws_codex.state, WiringState::Absent),
            "codex/workspace should still be Absent"
        );

        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(fake_home).ok();
    }

    /// Hand-crafted partial state (MCP key present, no hooks) → Partial with
    /// correct present/missing.
    #[test]
    fn qq1_status_partial_claude_code_workspace() {
        let root = temp_root("qq1_status_partial");
        let claude_dir = root.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Write only the MCP config (no hooks, no CLAUDE.md, no commands, no agent).
        let mcp = root.join(".mcp.json");
        fs::write(
            &mcp,
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": { "kimetsu": { "command": "kimetsu", "args": ["mcp", "serve"] } }
            }))
            .unwrap(),
        )
        .unwrap();

        let statuses = plugin_status_inner(&root);
        let ws_claude = statuses
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("claude-code/workspace");

        assert!(
            matches!(ws_claude.state, WiringState::Partial),
            "should be Partial; present={:?} missing={:?}",
            ws_claude.present,
            ws_claude.missing
        );
        assert!(
            ws_claude.present.contains(&"mcp".to_string()),
            "mcp should be present"
        );
        assert!(
            ws_claude.missing.contains(&"hooks".to_string()),
            "hooks should be missing"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Codex workspace: after install → Installed; partial (only MCP) → Partial.
    #[test]
    fn qq1_status_codex_workspace_install_and_partial() {
        let root = temp_root("qq1_status_codex");

        // Full install.
        plugin_install_inner(
            &root,
            BridgeTarget::Codex,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
            None,
        )
        .expect("codex install");

        let statuses = plugin_status_inner(&root);
        let ws_codex = statuses
            .iter()
            .find(|s| s.host == "codex" && s.scope == "workspace")
            .expect("codex/workspace");

        assert!(
            matches!(ws_codex.state, WiringState::Installed),
            "codex/workspace should be Installed; present={:?} missing={:?}",
            ws_codex.present,
            ws_codex.missing
        );
        assert!(ws_codex.present.contains(&"hooks".to_string()));
        assert!(ws_codex.present.contains(&"mcp".to_string()));
        assert!(ws_codex.present.contains(&"skill".to_string()));
        assert!(ws_codex.present.contains(&"agent".to_string()));

        // Now create a fresh workspace with only codex config.toml (no hooks).
        let root2 = temp_root("qq1_status_codex_partial");
        let codex_dir = root2.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        let config = codex_dir.join("config.toml");
        fs::write(
            &config,
            "[mcp_servers.kimetsu]\ncommand = \"kimetsu\"\nargs = [\"mcp\", \"serve\"]\n",
        )
        .unwrap();

        let statuses2 = plugin_status_inner(&root2);
        let partial = statuses2
            .iter()
            .find(|s| s.host == "codex" && s.scope == "workspace")
            .expect("codex/workspace partial");

        assert!(
            matches!(partial.state, WiringState::Partial),
            "should be Partial (only mcp); present={:?} missing={:?}",
            partial.present,
            partial.missing
        );
        assert!(partial.present.contains(&"mcp".to_string()));
        assert!(partial.missing.contains(&"hooks".to_string()));

        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(root2).ok();
    }

    /// User-only hooks/servers (non-kimetsu) don't make it report Installed.
    #[test]
    fn qq1_status_user_content_not_detected_as_kimetsu() {
        let root = temp_root("qq1_status_user_content");
        let claude_dir = root.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();

        // Write settings.json with a non-kimetsu hook.
        fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [
                        { "matcher": "", "hooks": [{ "type": "command", "command": "my-own-tool" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // Write .mcp.json with a non-kimetsu server.
        fs::write(
            root.join(".mcp.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": { "my-server": { "command": "my-server" } }
            }))
            .unwrap(),
        )
        .unwrap();

        let statuses = plugin_status_inner(&root);
        let ws_claude = statuses
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("claude-code/workspace");

        assert!(
            matches!(ws_claude.state, WiringState::Absent),
            "user-only hooks/servers must not register as Kimetsu wiring"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Install (ClaudeCode workspace) → status Installed → uninstall → status Absent.
    /// Running uninstall again (idempotent) → still Absent, no error.
    #[test]
    fn qq1_status_install_then_uninstall_flips_to_absent() {
        let root = temp_root("qq1_status_uninstall");

        plugin_install_inner(
            &root,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            true,
            None,
        )
        .expect("install");

        // Confirm Installed.
        let before = plugin_status_inner(&root);
        let ws = before
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("ws entry");
        assert!(
            matches!(ws.state, WiringState::Installed),
            "should be Installed before uninstall"
        );

        // Uninstall.
        plugin_uninstall_inner(
            &root,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            None,
        )
        .expect("uninstall");

        // Confirm Absent.
        let after = plugin_status_inner(&root);
        let ws2 = after
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("ws entry after uninstall");
        assert!(
            matches!(ws2.state, WiringState::Absent),
            "should be Absent after uninstall; present={:?}",
            ws2.present
        );

        // Idempotent second uninstall — no error.
        let result = plugin_uninstall_inner(
            &root,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            None,
        );
        assert!(result.is_ok(), "second uninstall should be a clean no-op");
        let after2 = plugin_status_inner(&root);
        let ws3 = after2
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace")
            .expect("ws entry after 2nd uninstall");
        assert!(matches!(ws3.state, WiringState::Absent), "still Absent");

        fs::remove_dir_all(root).ok();
    }

    // ── B-series: Pi host target ───────────────────────────────────────────────

    #[cfg(feature = "pi")]
    #[test]
    fn bridge_target_pi_parse_and_round_trip() {
        assert_eq!(BridgeTarget::parse("pi").unwrap(), BridgeTarget::Pi);
        assert_eq!(BridgeTarget::parse("PI").unwrap(), BridgeTarget::Pi);
        assert_eq!(BridgeTarget::Pi.as_str(), "pi");
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_install_workspace_writes_expected_files() {
        let ws = temp_root("pi_install_ws");

        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None, // workspace scope — no home injection
        )
        .expect("Pi workspace install");

        let pi = ws.join(".pi");
        assert!(pi.join("extensions/kimetsu.ts").is_file(), "extension ts");
        assert!(pi.join("settings.json").is_file(), "settings.json");
        assert!(
            pi.join("skills/kimetsu-brain/SKILL.md").is_file(),
            "SKILL.md"
        );

        // settings.json must register the extension path.
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pi.join("settings.json")).unwrap()).unwrap();
        let exts = settings["extensions"].as_array().unwrap();
        assert!(
            exts.iter()
                .any(|v| v.as_str() == Some("./extensions/kimetsu.ts")),
            "kimetsu.ts registered in extensions array"
        );

        // Extension TS must not panic Pi if kimetsu is absent (silent no-op comment present).
        let ts = fs::read_to_string(pi.join("extensions/kimetsu.ts")).unwrap();
        assert!(
            ts.contains("silent no-op") || ts.contains("silent"),
            "silent no-op on missing binary"
        );
        assert!(ts.contains("session_start"), "hooks session_start");
        assert!(ts.contains("agent_end"), "hooks agent_end");
        assert!(ts.contains("session_shutdown"), "hooks session_shutdown");

        fs::remove_dir_all(ws).ok();
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_install_workspace_is_idempotent() {
        let ws = temp_root("pi_install_idem");

        for _ in 0..2 {
            plugin_install_inner(
                &ws,
                BridgeTarget::Pi,
                InstallScope::Workspace,
                PluginMode::Optional,
                false,
                false,
                None,
            )
            .expect("Pi workspace install (idempotent)");
        }

        // settings.json must NOT have duplicate entries.
        let pi = ws.join(".pi");
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pi.join("settings.json")).unwrap()).unwrap();
        let exts = settings["extensions"].as_array().unwrap();
        let count = exts
            .iter()
            .filter(|v| v.as_str() == Some("./extensions/kimetsu.ts"))
            .count();
        assert_eq!(count, 1, "no duplicate registration after re-install");

        fs::remove_dir_all(ws).ok();
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_install_global_writes_to_home_not_workspace() {
        let ws = temp_root("pi_install_global_ws");
        let home = temp_root("pi_install_global_home");

        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Global,
            PluginMode::Optional,
            false,
            false,
            Some(home.as_path()),
        )
        .expect("Pi global install");

        // Files must be under ~/.pi/agent/, not under workspace.
        let pi_agent = home.join(".pi").join("agent");
        assert!(
            pi_agent.join("extensions/kimetsu.ts").is_file(),
            "global extension ts"
        );
        assert!(
            pi_agent.join("settings.json").is_file(),
            "global settings.json"
        );
        assert!(
            pi_agent.join("skills/kimetsu-brain/SKILL.md").is_file(),
            "global SKILL.md"
        );
        assert!(!ws.join(".pi").exists(), "workspace must be untouched");

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_detect_helpers_false_before_true_after_install() {
        let ws = temp_root("pi_detect");
        let pi_dir = ws.join(".pi");
        fs::create_dir_all(&pi_dir).unwrap();

        // Before install: both false.
        assert!(!detect_pi_extension(&pi_dir), "no extension before install");
        assert!(!detect_pi_skill(&pi_dir), "no skill before install");

        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        // After install: both true.
        assert!(
            detect_pi_extension(&pi_dir),
            "extension detected after install"
        );
        assert!(detect_pi_skill(&pi_dir), "skill detected after install");

        fs::remove_dir_all(ws).ok();
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_status_fully_installed_reports_installed() {
        let ws = temp_root("pi_status_installed");

        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        let statuses = plugin_status_inner(&ws);
        let pi_ws = statuses
            .iter()
            .find(|s| s.host == "pi" && s.scope == "workspace")
            .expect("pi workspace status entry");

        assert!(
            matches!(pi_ws.state, WiringState::Installed),
            "fully installed Pi reports Installed, got: {:?} (present={:?}, missing={:?})",
            pi_ws.state,
            pi_ws.present,
            pi_ws.missing
        );

        fs::remove_dir_all(ws).ok();
    }

    #[test]
    fn aggregate_state_extension_counts_as_core() {
        // "extension" alone present with "skill" missing → Partial (not Absent).
        let state = aggregate_state(&["extension"], &["skill"]);
        assert!(
            matches!(state, WiringState::Partial),
            "extension is a core piece: partial when skill missing"
        );

        // Both present → Installed.
        let state2 = aggregate_state(&["extension", "skill"], &[]);
        assert!(matches!(state2, WiringState::Installed));

        // Nothing present → Absent.
        let state3 = aggregate_state(&[], &["extension", "skill"]);
        assert!(matches!(state3, WiringState::Absent));

        // "plugin" also counts as core.
        let state4 = aggregate_state(&["plugin"], &["other"]);
        assert!(matches!(state4, WiringState::Partial));
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_uninstall_removes_files_and_strips_settings() {
        let ws = temp_root("pi_uninstall");

        // Install first.
        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        // Add a user key to settings.json to confirm it is preserved.
        let settings_path = ws.join(".pi/settings.json");
        let mut settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        settings["userKey"] = serde_json::Value::String("preserved".to_string());
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        // Uninstall.
        let report = plugin_uninstall_inner(&ws, BridgeTarget::Pi, InstallScope::Workspace, None)
            .expect("Pi uninstall");

        let pi = ws.join(".pi");
        assert!(
            !pi.join("extensions/kimetsu.ts").exists(),
            "extension removed"
        );
        assert!(
            !pi.join("skills/kimetsu-brain").exists(),
            "skill dir removed"
        );
        assert!(
            !report.removed.is_empty() || !report.modified.is_empty(),
            "something changed"
        );

        // settings.json should still exist with userKey intact, kimetsu entry stripped.
        assert!(settings_path.is_file(), "settings.json still exists");
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            after["userKey"].as_str(),
            Some("preserved"),
            "user key preserved"
        );
        // extensions array should be gone (was empty after stripping).
        let exts_has_kimetsu = after
            .get("extensions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .any(|v| v.as_str() == Some("./extensions/kimetsu.ts"))
            })
            .unwrap_or(false);
        assert!(!exts_has_kimetsu, "kimetsu entry stripped from extensions");

        fs::remove_dir_all(ws).ok();
    }

    #[cfg(feature = "pi")]
    #[test]
    fn pi_uninstall_is_idempotent() {
        let ws = temp_root("pi_uninstall_idem");

        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        // First uninstall.
        plugin_uninstall_inner(&ws, BridgeTarget::Pi, InstallScope::Workspace, None).unwrap();

        // Second uninstall — must be a clean no-op.
        let result = plugin_uninstall_inner(&ws, BridgeTarget::Pi, InstallScope::Workspace, None);
        assert!(result.is_ok(), "second Pi uninstall is a clean no-op");

        fs::remove_dir_all(ws).ok();
    }

    #[cfg(feature = "pi")]
    #[test]
    fn bridge_export_skill_pi_uses_dot_pi_skills() {
        let ws = temp_root("pi_export_skill");
        // Create a minimal skill for exporting.
        let skill_src = ws.join(".kimetsu/extensions/reviewer");
        fs::create_dir_all(&skill_src).unwrap();
        fs::write(
            skill_src.join("manifest.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "reviewer",
                "name": "reviewer",
                "description": "Review code.",
                "kind": "skill",
                "source": "kimetsu",
                "origin": "kimetsu",
                "imported_at_unix": 0u64,
                "capabilities": []
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            skill_src.join("SKILL.md"),
            "---\nname: reviewer\ndescription: Review.\n---\nLead.",
        )
        .unwrap();

        let config = SkillConfig::default();
        let dest = bridge_export_skill(&ws, &config, "reviewer", BridgeTarget::Pi, false)
            .expect("Pi export skill");
        // Destination should be .pi/skills/reviewer
        assert!(
            dest.to_string_lossy().contains(".pi") && dest.to_string_lossy().contains("reviewer"),
            "Pi export writes to .pi/skills/reviewer, got: {}",
            dest.display()
        );

        fs::remove_dir_all(ws).ok();
    }

    // -------------------------------------------------------------------------
    // C-tests — OpenClaw host target
    // -------------------------------------------------------------------------

    /// C2: BridgeTarget parse/as_str round-trip for openclaw/claw aliases.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c2_openclaw_bridge_target_parse_and_as_str() {
        assert_eq!(
            BridgeTarget::parse("openclaw").unwrap(),
            BridgeTarget::OpenClaw
        );
        assert_eq!(BridgeTarget::parse("claw").unwrap(), BridgeTarget::OpenClaw);
        assert_eq!(
            BridgeTarget::parse("OPENCLAW").unwrap(),
            BridgeTarget::OpenClaw
        );
        assert_eq!(BridgeTarget::OpenClaw.as_str(), "openclaw");
    }

    /// C4: workspace install writes openclaw.json with mcp.servers.kimetsu + plugins.entries.kimetsu,
    /// plugins/kimetsu/index.ts, plugins/kimetsu/openclaw.plugin.json, and
    /// workspace/skills/kimetsu-context/SKILL.md. Re-run is idempotent.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c4_install_openclaw_workspace_writes_all_files_and_is_idempotent() {
        let ws = temp_root("c4_openclaw_ws");

        // First install.
        let report = plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None, // workspace install
        )
        .expect("openclaw workspace install");

        let oc_dir = ws.join(".openclaw");

        // openclaw.json has mcp.servers.kimetsu.
        let config_text = fs::read_to_string(oc_dir.join("openclaw.json")).expect("openclaw.json");
        let config: serde_json::Value =
            serde_json::from_str(&config_text).expect("openclaw.json parse");
        assert_eq!(
            config["mcp"]["servers"]["kimetsu"]["command"], "kimetsu",
            "mcp.servers.kimetsu.command must be 'kimetsu'"
        );
        assert_eq!(
            config["mcp"]["servers"]["kimetsu"]["args"][0], "mcp",
            "args[0] must be 'mcp'"
        );

        // openclaw.json has plugins.entries.kimetsu.
        assert!(
            config["plugins"]["entries"]["kimetsu"].is_object(),
            "plugins.entries.kimetsu must be present"
        );

        // Plugin files exist.
        assert!(
            oc_dir.join("plugins/kimetsu/index.ts").is_file(),
            "plugins/kimetsu/index.ts must exist"
        );
        assert!(
            oc_dir
                .join("plugins/kimetsu/openclaw.plugin.json")
                .is_file(),
            "plugins/kimetsu/openclaw.plugin.json must exist"
        );

        // Skill file exists.
        assert!(
            oc_dir
                .join("workspace/skills/kimetsu-context/SKILL.md")
                .is_file(),
            "workspace/skills/kimetsu-context/SKILL.md must exist"
        );

        // Report lists the files.
        assert!(
            report.files.len() >= 4,
            "report should list at least 4 files"
        );

        // Idempotent: second install must succeed with no error.
        plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("openclaw workspace install second run");

        // After second install, still only one kimetsu server entry.
        let config2_text =
            fs::read_to_string(oc_dir.join("openclaw.json")).expect("openclaw.json 2nd");
        let config2: serde_json::Value = serde_json::from_str(&config2_text).expect("parse 2nd");
        assert_eq!(
            config2["mcp"]["servers"]
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| k.as_str() == "kimetsu")
                .count(),
            1,
            "exactly one kimetsu server entry after two installs"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// C4 (merge): pre-seed openclaw.json with comments and a non-Kimetsu MCP server,
    /// then install. After install, the other server must survive AND comments-lost
    /// note must be in the install report.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c4_install_openclaw_merges_into_preseeded_json5_config() {
        let ws = temp_root("c4_openclaw_merge");
        let oc_dir = ws.join(".openclaw");
        fs::create_dir_all(&oc_dir).unwrap();

        // Seed a JSON5 config with comments and an existing MCP server.
        // json5::from_str can parse this; after install it will be reformatted
        // as plain JSON (comments lost).
        let seed = r#"{
  // My OpenClaw configuration
  "mcp": {
    "servers": {
      // Other server I rely on
      "other": { "command": "other-server", "args": [] }
    }
  },
  "agent": {
    "model": "anthropic/claude-3-5-sonnet",  // my preferred model
  }
}"#;
        fs::write(oc_dir.join("openclaw.json"), seed).unwrap();

        let report = plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("install into pre-seeded config");

        let config_text = fs::read_to_string(oc_dir.join("openclaw.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&config_text).unwrap();

        // Kimetsu server was added.
        assert_eq!(
            config["mcp"]["servers"]["kimetsu"]["command"], "kimetsu",
            "kimetsu server added"
        );

        // Existing server survived.
        assert_eq!(
            config["mcp"]["servers"]["other"]["command"], "other-server",
            "pre-existing 'other' server preserved"
        );

        // Unrelated key survived.
        assert!(
            config["agent"]["model"].as_str().is_some(),
            "agent.model key preserved"
        );

        // Note about comment loss is in the report.
        assert!(
            report.notes.iter().any(|n| n.contains("reformatted")),
            "install report must note that comments were not preserved"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// C4 (global): install into injected home → writes under <home>/.openclaw/.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c4_install_openclaw_global_writes_under_home() {
        let ws = temp_root("c4_openclaw_global_ws");
        let home = temp_root("c4_openclaw_global_home");

        plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Global,
            PluginMode::Optional,
            false,
            false,
            Some(home.as_path()),
        )
        .expect("openclaw global install");

        let oc_dir = home.join(".openclaw");
        assert!(
            oc_dir.join("openclaw.json").is_file(),
            "global openclaw.json"
        );
        assert!(
            oc_dir.join("plugins/kimetsu/index.ts").is_file(),
            "global plugin ts"
        );
        assert!(
            oc_dir
                .join("workspace/skills/kimetsu-context/SKILL.md")
                .is_file(),
            "global skill"
        );

        // Workspace must be untouched.
        assert!(
            !ws.join(".openclaw").exists(),
            "workspace .openclaw must not be created"
        );

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    /// C5: detect_openclaw_* returns false before install, true after.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c5_detect_openclaw_false_before_true_after_install() {
        let ws = temp_root("c5_detect_openclaw");
        let oc_dir = ws.join(".openclaw");

        // Before: all detectors return false.
        assert!(!detect_openclaw_mcp(&oc_dir), "mcp false before install");
        assert!(
            !detect_openclaw_plugin(&oc_dir),
            "plugin false before install"
        );
        assert!(
            !detect_openclaw_skill(&oc_dir),
            "skill false before install"
        );

        plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        // After: all detectors return true.
        assert!(detect_openclaw_mcp(&oc_dir), "mcp true after install");
        assert!(detect_openclaw_plugin(&oc_dir), "plugin true after install");
        assert!(detect_openclaw_skill(&oc_dir), "skill true after install");

        fs::remove_dir_all(ws).ok();
    }

    /// C6: status returns WiringState::Installed when fully wired.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c6_status_openclaw_fully_installed_is_installed() {
        let ws = temp_root("c6_status_openclaw");

        plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        let statuses = plugin_status_inner(&ws);
        let oc_ws = statuses
            .iter()
            .find(|s| s.host == "openclaw" && s.scope == "workspace")
            .expect("openclaw workspace status must be present");

        assert!(
            matches!(oc_ws.state, WiringState::Installed),
            "fully installed openclaw must be WiringState::Installed, got {:?}",
            oc_ws.state
        );
        assert!(oc_ws.present.contains(&"mcp".to_string()));
        assert!(oc_ws.present.contains(&"plugin".to_string()));
        assert!(oc_ws.present.contains(&"skill".to_string()));
        assert!(oc_ws.missing.is_empty());

        fs::remove_dir_all(ws).ok();
    }

    /// C7: uninstall removes kimetsu mcp/plugin/skill, preserves 'other' server,
    /// and second uninstall is a clean no-op.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c7_uninstall_openclaw_removes_kimetsu_preserves_other_and_is_idempotent() {
        let ws = temp_root("c7_uninstall_openclaw");
        let oc_dir = ws.join(".openclaw");

        // First install.
        plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .unwrap();

        // Seed an additional server so we can verify it survives uninstall.
        let config_text = fs::read_to_string(oc_dir.join("openclaw.json")).unwrap();
        let mut config: serde_json::Value = serde_json::from_str(&config_text).unwrap();
        config["mcp"]["servers"]["other"] = json!({ "command": "other-server" });
        fs::write(
            oc_dir.join("openclaw.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        // Uninstall.
        let report =
            plugin_uninstall_inner(&ws, BridgeTarget::OpenClaw, InstallScope::Workspace, None)
                .expect("openclaw uninstall");

        // Modified: openclaw.json was edited.
        assert!(
            report
                .modified
                .iter()
                .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("openclaw.json")),
            "openclaw.json should appear in modified list"
        );

        // kimetsu MCP entry removed.
        let after_text = fs::read_to_string(oc_dir.join("openclaw.json")).unwrap();
        let after: serde_json::Value = serde_json::from_str(&after_text).unwrap();
        assert!(
            after["mcp"]["servers"]
                .as_object()
                .map(|m| !m.contains_key("kimetsu"))
                .unwrap_or(true),
            "kimetsu must be removed from mcp.servers"
        );

        // 'other' server survived.
        assert_eq!(
            after["mcp"]["servers"]["other"]["command"], "other-server",
            "'other' server must survive uninstall"
        );

        // Plugin dir and skill dir removed.
        assert!(
            !oc_dir.join("plugins/kimetsu").exists(),
            "plugins/kimetsu must be deleted"
        );
        assert!(
            !oc_dir.join("workspace/skills/kimetsu-context").exists(),
            "workspace/skills/kimetsu-context must be deleted"
        );

        // Second uninstall is a clean no-op.
        let result2 =
            plugin_uninstall_inner(&ws, BridgeTarget::OpenClaw, InstallScope::Workspace, None);
        assert!(
            result2.is_ok(),
            "second openclaw uninstall is a clean no-op"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// C8: bridge_export_skill for OpenClaw writes to .openclaw/workspace/skills/<name>.
    #[cfg(feature = "openclaw")]
    #[test]
    fn c8_bridge_export_skill_openclaw_uses_workspace_skills() {
        let ws = temp_root("c8_openclaw_export_skill");
        // Create a minimal skill for exporting.
        let skill_src = ws.join(".kimetsu/extensions/my-skill");
        fs::create_dir_all(&skill_src).unwrap();
        fs::write(
            skill_src.join("manifest.json"),
            serde_json::to_string(&serde_json::json!({
                "id": "my-skill",
                "name": "my-skill",
                "description": "Test skill.",
                "kind": "skill",
                "source": "kimetsu",
                "origin": "kimetsu",
                "imported_at_unix": 0u64,
                "capabilities": []
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            skill_src.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Test.\n---\nContent.",
        )
        .unwrap();

        let config = SkillConfig::default();
        let dest = bridge_export_skill(&ws, &config, "my-skill", BridgeTarget::OpenClaw, false)
            .expect("OpenClaw export skill");

        let dest_str = dest.to_string_lossy();
        assert!(
            dest_str.contains(".openclaw")
                && dest_str.contains("workspace")
                && dest_str.contains("skills"),
            "OpenClaw export writes to .openclaw/workspace/skills/, got: {}",
            dest.display()
        );
        assert!(
            dest.join("SKILL.md").is_file(),
            "SKILL.md must exist in dest"
        );

        fs::remove_dir_all(ws).ok();
    }

    // ── Warm-daemon startup hook tests ────────────────────────────────────────

    /// Claude Code settings.json must include a SessionStart group that warms
    /// the embedder daemon. The group must survive idempotent re-runs.
    #[test]
    fn claude_hooks_include_sessionstart_warm() {
        let root = temp_root("claude_sessionstart_warm");
        let claude = root.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");

        write_claude_hooks(&settings, false).expect("write_claude_hooks");

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        let ss = value["hooks"]["SessionStart"]
            .as_array()
            .expect("SessionStart array");
        assert!(
            ss.iter()
                .any(|g| g["hooks"][0]["command"] == "kimetsu brain warm"),
            "SessionStart must warm the embedder daemon"
        );

        // Idempotent: second run must not add a second group.
        write_claude_hooks(&settings, false).expect("second write_claude_hooks");
        let value2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        let ss2 = value2["hooks"]["SessionStart"].as_array().unwrap();
        let warm_count = ss2
            .iter()
            .filter(|g| g["hooks"][0]["command"] == "kimetsu brain warm")
            .count();
        assert_eq!(
            warm_count, 1,
            "exactly one SessionStart warm group after two runs"
        );

        fs::remove_dir_all(root).ok();
    }

    /// Pi extension TS must call kimetsuExec(["brain", "warm"]) inside the
    /// session_start handler.
    #[cfg(feature = "pi")]
    #[test]
    fn pi_extension_ts_session_start_includes_warm() {
        let ws = temp_root("pi_warm_sessionstart");
        plugin_install_inner(
            &ws,
            BridgeTarget::Pi,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("Pi workspace install");

        let ts = fs::read_to_string(ws.join(".pi/extensions/kimetsu.ts")).unwrap();
        assert!(
            ts.contains("\"brain\", \"warm\""),
            "Pi session_start handler must warm the embedder daemon"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// OpenClaw plugin TS must call kimetsuExec(["brain", "warm"]) at startup
    /// (inside register(), outside any event handler).
    #[cfg(feature = "openclaw")]
    #[test]
    fn openclaw_plugin_ts_register_includes_warm() {
        let ws = temp_root("openclaw_warm_startup");
        plugin_install_inner(
            &ws,
            BridgeTarget::OpenClaw,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("OpenClaw workspace install");

        let ts = fs::read_to_string(ws.join(".openclaw/plugins/kimetsu/index.ts")).unwrap();
        assert!(
            ts.contains("\"brain\", \"warm\""),
            "OpenClaw plugin register() must warm the embedder daemon at startup"
        );

        fs::remove_dir_all(ws).ok();
    }

    // -------------------------------------------------------------------------
    // Cursor — workspace + global install/uninstall/status tests
    // -------------------------------------------------------------------------

    /// Cursor workspace install writes `.cursor/mcp.json` with `type: "stdio"`
    /// and a rules file at `.cursor/rules/kimetsu-brain/rule.md`.
    #[test]
    fn cursor_workspace_install_writes_mcp_and_rules() {
        let ws = temp_root("cursor_ws_install");

        plugin_install_inner(
            &ws,
            BridgeTarget::Cursor,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("Cursor workspace install");

        let mcp_path = ws.join(".cursor/mcp.json");
        assert!(mcp_path.is_file(), ".cursor/mcp.json must exist");
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kimetsu"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");
        let args = v["mcpServers"]["kimetsu"]["args"].as_array().unwrap();
        assert!(
            args.iter().any(|a| a == "serve"),
            "args must include 'serve'"
        );

        let rule_path = ws.join(".cursor/rules/kimetsu-brain/rule.md");
        assert!(rule_path.is_file(), "Cursor rule file must exist");
        let rule_text = fs::read_to_string(&rule_path).unwrap();
        assert!(
            rule_text.contains("alwaysApply: true"),
            "rule must have alwaysApply frontmatter"
        );
        assert!(rule_text.contains("Kimetsu"), "rule must mention Kimetsu");

        fs::remove_dir_all(ws).ok();
    }

    /// Cursor workspace install is idempotent: a second run must not error and
    /// must not duplicate entries in `.cursor/mcp.json`.
    #[test]
    fn cursor_workspace_install_is_idempotent() {
        let ws = temp_root("cursor_ws_idem");

        for _ in 0..2 {
            plugin_install_inner(
                &ws,
                BridgeTarget::Cursor,
                InstallScope::Workspace,
                PluginMode::Optional,
                false,
                false,
                None,
            )
            .expect("Cursor install must be idempotent");
        }

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ws.join(".cursor/mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["mcpServers"].as_object().unwrap().len(),
            1,
            "exactly one entry in mcpServers — no duplicates"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Cursor workspace install preserves a pre-existing user MCP server.
    #[test]
    fn cursor_workspace_install_preserves_user_server() {
        let ws = temp_root("cursor_ws_preserve");
        let cursor_dir = ws.join(".cursor");
        fs::create_dir_all(&cursor_dir).unwrap();
        fs::write(
            cursor_dir.join("mcp.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "my-server": { "type": "stdio", "command": "my-cmd", "args": [] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        plugin_install_inner(
            &ws,
            BridgeTarget::Cursor,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("Cursor install with pre-seeded mcp.json");

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cursor_dir.join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["mcpServers"]["my-server"]["command"], "my-cmd",
            "user server must survive"
        );
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");

        fs::remove_dir_all(ws).ok();
    }

    /// Cursor global install writes into `~/.cursor/mcp.json` (injected home).
    #[test]
    fn cursor_global_install_writes_to_home() {
        let ws = temp_root("cursor_global_ws");
        let home = temp_root("cursor_global_home");

        plugin_install_inner(
            &ws,
            BridgeTarget::Cursor,
            InstallScope::Global,
            PluginMode::Optional,
            false,
            false,
            Some(home.as_path()),
        )
        .expect("Cursor global install");

        let mcp_path = home.join(".cursor/mcp.json");
        assert!(
            mcp_path.is_file(),
            "~/.cursor/mcp.json must exist for global install"
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mcp_path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");
        // Workspace directory must remain untouched.
        assert!(
            !ws.join(".cursor").exists(),
            "workspace .cursor must not exist for global install"
        );

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    /// Cursor uninstall removes `mcpServers.kimetsu` from `.cursor/mcp.json`.
    #[test]
    fn cursor_uninstall_removes_mcp_entry() {
        let ws = temp_root("cursor_uninstall");

        // Install first.
        plugin_install_inner(
            &ws,
            BridgeTarget::Cursor,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("install");

        let cursor_dir = ws.join(".cursor");
        assert!(detect_cursor_mcp(&cursor_dir));

        // Uninstall.
        let report = plugin_uninstall(&ws, BridgeTarget::Cursor, InstallScope::Workspace)
            .expect("Cursor uninstall");

        assert!(
            !detect_cursor_mcp(&cursor_dir),
            "mcp entry must be gone after uninstall"
        );
        assert!(
            !report.modified.is_empty(),
            "report must list modified files"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// `plugin_status` detects an installed Cursor workspace entry.
    #[test]
    fn cursor_status_detects_installed_workspace() {
        let ws = temp_root("cursor_status_ws");

        plugin_install_inner(
            &ws,
            BridgeTarget::Cursor,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("install");

        let statuses = plugin_status_inner(&ws);
        let entry = statuses
            .iter()
            .find(|s| s.host == "cursor" && s.scope == "workspace");
        assert!(entry.is_some(), "cursor/workspace status entry must exist");
        let entry = entry.unwrap();
        assert!(
            matches!(entry.state, WiringState::Installed),
            "state must be Installed, got {:?}",
            entry.state
        );

        fs::remove_dir_all(ws).ok();
    }

    // -------------------------------------------------------------------------
    // Gemini CLI — workspace + global install/uninstall/status tests
    // -------------------------------------------------------------------------

    /// Gemini CLI workspace install writes `.gemini/settings.json` (mcpServers)
    /// and merges a `GEMINI.md` block at the project root.
    #[test]
    fn gemini_workspace_install_writes_settings_and_md() {
        let ws = temp_root("gemini_ws_install");

        plugin_install_inner(
            &ws,
            BridgeTarget::GeminiCli,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("Gemini CLI workspace install");

        let settings_path = ws.join(".gemini/settings.json");
        assert!(settings_path.is_file(), ".gemini/settings.json must exist");
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");
        let args = v["mcpServers"]["kimetsu"]["args"].as_array().unwrap();
        assert!(
            args.iter().any(|a| a == "serve"),
            "args must include 'serve'"
        );

        let gemini_md_path = ws.join("GEMINI.md");
        assert!(
            gemini_md_path.is_file(),
            "GEMINI.md must exist at workspace root"
        );
        let md_text = fs::read_to_string(&gemini_md_path).unwrap();
        assert!(
            md_text.contains(GEMINI_MD_BEGIN),
            "GEMINI.md must have begin marker"
        );
        assert!(
            md_text.contains("Kimetsu"),
            "GEMINI.md must mention Kimetsu"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Gemini CLI workspace install is idempotent.
    #[test]
    fn gemini_workspace_install_is_idempotent() {
        let ws = temp_root("gemini_ws_idem");

        for _ in 0..2 {
            plugin_install_inner(
                &ws,
                BridgeTarget::GeminiCli,
                InstallScope::Workspace,
                PluginMode::Optional,
                false,
                false,
                None,
            )
            .expect("Gemini install must be idempotent");
        }

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ws.join(".gemini/settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["mcpServers"].as_object().unwrap().len(),
            1,
            "exactly one entry in mcpServers"
        );

        let md_text = fs::read_to_string(ws.join("GEMINI.md")).unwrap();
        assert_eq!(
            md_text.matches(GEMINI_MD_BEGIN).count(),
            1,
            "GEMINI.md must have exactly one Kimetsu block after two installs"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Gemini CLI workspace install preserves a pre-existing user MCP server.
    #[test]
    fn gemini_workspace_install_preserves_user_server() {
        let ws = temp_root("gemini_ws_preserve");
        let gemini_dir = ws.join(".gemini");
        fs::create_dir_all(&gemini_dir).unwrap();
        fs::write(
            gemini_dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "my-tool": { "command": "my-tool-cmd", "args": [] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        plugin_install_inner(
            &ws,
            BridgeTarget::GeminiCli,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("Gemini install with pre-seeded settings.json");

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(gemini_dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["mcpServers"]["my-tool"]["command"], "my-tool-cmd",
            "user tool must survive"
        );
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");

        fs::remove_dir_all(ws).ok();
    }

    /// Gemini CLI workspace install preserves pre-existing user content in GEMINI.md.
    #[test]
    fn gemini_workspace_install_preserves_gemini_md_user_content() {
        let ws = temp_root("gemini_ws_md_preserve");
        // Pre-seed a GEMINI.md with user instructions.
        fs::write(ws.join("GEMINI.md"), "# Project rules\nAlways test.\n").unwrap();

        plugin_install_inner(
            &ws,
            BridgeTarget::GeminiCli,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("Gemini install with pre-seeded GEMINI.md");

        let text = fs::read_to_string(ws.join("GEMINI.md")).unwrap();
        assert!(
            text.contains("# Project rules"),
            "user content must survive"
        );
        assert!(text.contains("Always test."), "user detail must survive");
        assert!(text.contains("Kimetsu"), "kimetsu block appended");
        assert!(
            text.find("# Project rules").unwrap() < text.find(GEMINI_MD_BEGIN).unwrap(),
            "user content must precede kimetsu block"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// Gemini CLI global install writes to `~/.gemini/settings.json` and
    /// `~/.gemini/GEMINI.md` (injected home).
    #[test]
    fn gemini_global_install_writes_to_home() {
        let ws = temp_root("gemini_global_ws");
        let home = temp_root("gemini_global_home");

        plugin_install_inner(
            &ws,
            BridgeTarget::GeminiCli,
            InstallScope::Global,
            PluginMode::Optional,
            false,
            false,
            Some(home.as_path()),
        )
        .expect("Gemini CLI global install");

        let settings_path = home.join(".gemini/settings.json");
        assert!(
            settings_path.is_file(),
            "~/.gemini/settings.json must exist"
        );
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");

        let gemini_md_path = home.join(".gemini/GEMINI.md");
        assert!(
            gemini_md_path.is_file(),
            "~/.gemini/GEMINI.md must exist for global install"
        );
        let md_text = fs::read_to_string(&gemini_md_path).unwrap();
        assert!(md_text.contains(GEMINI_MD_BEGIN));

        // Workspace directory must remain untouched.
        assert!(
            !ws.join(".gemini").exists(),
            "workspace .gemini must not exist for global install"
        );
        assert!(
            !ws.join("GEMINI.md").exists(),
            "workspace GEMINI.md must not exist for global install"
        );

        fs::remove_dir_all(ws).ok();
        fs::remove_dir_all(home).ok();
    }

    /// Gemini CLI uninstall removes `mcpServers.kimetsu` from
    /// `.gemini/settings.json` and strips the kimetsu block from `GEMINI.md`.
    #[test]
    fn gemini_uninstall_removes_settings_and_md() {
        let ws = temp_root("gemini_uninstall");

        // Install first.
        plugin_install_inner(
            &ws,
            BridgeTarget::GeminiCli,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("install");

        assert!(detect_gemini_mcp(&ws.join(".gemini")));
        assert!(detect_gemini_md(&ws));

        // Uninstall.
        let report = plugin_uninstall(&ws, BridgeTarget::GeminiCli, InstallScope::Workspace)
            .expect("Gemini CLI uninstall");

        assert!(
            !detect_gemini_mcp(&ws.join(".gemini")),
            "mcp entry must be gone after uninstall"
        );
        assert!(
            !detect_gemini_md(&ws),
            "GEMINI.md block must be gone after uninstall"
        );
        assert!(
            !report.modified.is_empty(),
            "report must list modified files"
        );

        fs::remove_dir_all(ws).ok();
    }

    /// `plugin_status` detects an installed Gemini CLI workspace entry.
    #[test]
    fn gemini_status_detects_installed_workspace() {
        let ws = temp_root("gemini_status_ws");

        plugin_install_inner(
            &ws,
            BridgeTarget::GeminiCli,
            InstallScope::Workspace,
            PluginMode::Optional,
            false,
            false,
            None,
        )
        .expect("install");

        let statuses = plugin_status_inner(&ws);
        let entry = statuses
            .iter()
            .find(|s| s.host == "gemini-cli" && s.scope == "workspace");
        assert!(
            entry.is_some(),
            "gemini-cli/workspace status entry must exist"
        );
        let entry = entry.unwrap();
        assert!(
            matches!(entry.state, WiringState::Installed),
            "state must be Installed, got {:?}",
            entry.state
        );

        fs::remove_dir_all(ws).ok();
    }

    // -------------------------------------------------------------------------
    // merge_gemini_md — unit tests (mirrors merge_claude_md suite)
    // -------------------------------------------------------------------------

    #[test]
    fn merge_gemini_md_fresh_file() {
        let root = temp_root("gemini_md_fresh");
        let p = root.join("GEMINI.md");
        merge_gemini_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains(GEMINI_MD_BEGIN));
        assert!(text.contains("Kimetsu"));
        assert!(text.contains(GEMINI_MD_END));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_gemini_md_preserves_user_content() {
        let root = temp_root("gemini_md_preserve");
        let p = root.join("GEMINI.md");
        fs::write(&p, "# My rules\nAlways use tabs.\n").unwrap();
        merge_gemini_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("# My rules"));
        assert!(text.contains("Always use tabs."));
        assert!(text.contains("Kimetsu"));
        assert!(
            text.find("My rules").unwrap() < text.find(GEMINI_MD_BEGIN).unwrap(),
            "user content precedes the kimetsu block"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_gemini_md_idempotent() {
        let root = temp_root("gemini_md_idem");
        let p = root.join("GEMINI.md");
        fs::write(&p, "# Mine\n").unwrap();
        merge_gemini_md(&p).unwrap();
        merge_gemini_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert_eq!(
            text.matches(GEMINI_MD_BEGIN).count(),
            1,
            "no duplicate block"
        );
        assert_eq!(text.matches(GEMINI_MD_END).count(), 1);
        assert!(text.contains("# Mine"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn merge_gemini_md_upgrades_in_place() {
        let root = temp_root("gemini_md_upgrade");
        let p = root.join("GEMINI.md");
        fs::write(
            &p,
            format!("# Top\n\n{GEMINI_MD_BEGIN}\nOLD STALE\n{GEMINI_MD_END}\n\n# Bottom\n"),
        )
        .unwrap();
        merge_gemini_md(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(!text.contains("OLD STALE"), "stale block replaced");
        assert!(text.contains("Kimetsu"));
        assert!(text.contains("# Top"));
        assert!(text.contains("# Bottom"));
        assert_eq!(text.matches(GEMINI_MD_BEGIN).count(), 1);
        fs::remove_dir_all(root).ok();
    }

    // -------------------------------------------------------------------------
    // write_cursor_mcp_config / write_gemini_settings — unit tests
    // -------------------------------------------------------------------------

    #[test]
    fn write_cursor_mcp_config_fresh_and_idempotent() {
        let root = temp_root("cursor_mcp_unit");
        let path = root.join("mcp.json");
        write_cursor_mcp_config(&path).unwrap();
        write_cursor_mcp_config(&path).unwrap(); // idempotent
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kimetsu"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");
        assert_eq!(
            v["mcpServers"].as_object().unwrap().len(),
            1,
            "no duplicate entries"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn write_gemini_settings_fresh_and_idempotent() {
        let root = temp_root("gemini_settings_unit");
        let path = root.join("settings.json");
        write_gemini_settings(&path).unwrap();
        write_gemini_settings(&path).unwrap(); // idempotent
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["kimetsu"]["command"], "kimetsu");
        assert_eq!(
            v["mcpServers"].as_object().unwrap().len(),
            1,
            "no duplicate entries"
        );
        // Gemini CLI does NOT use `type: "stdio"` — just command + args.
        assert!(
            v["mcpServers"]["kimetsu"].get("type").is_none(),
            "Gemini CLI settings must not have a 'type' field"
        );
        fs::remove_dir_all(root).ok();
    }
}
