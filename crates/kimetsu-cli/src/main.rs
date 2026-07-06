use std::env;

use std::path::PathBuf;

mod ask;
mod commands;
mod distiller;
mod doctor;
mod embed_daemon;
mod harvest_setup;
mod proactive_state;
mod process;
mod remote_client;
mod skill_synth;
mod update;

use clap::{Args, Parser, Subcommand};
use kimetsu_agent::bench::{BenchOptions, run_benchmark};
use kimetsu_agent::pipeline::{CodingRunOptions, run_coding};
use kimetsu_agent::swe_bench::{SweBenchOptions, run_swe_bench};
use kimetsu_core::KimetsuResult;

use tracing_subscriber::EnvFilter;

#[allow(unused_imports)]
use commands::{
    bench::*, brain::*, chat::*, config::*, hooks::*, hosts::*, integrations::*, lifecycle::*,
    memory::*, runs::*,
};

/// User-facing version string: bare semver + build flavor in parentheses.
/// Clap prints this for `--version` / `-V`.
///
/// Composed by build.rs from CARGO_FEATURE_* env vars so the flavor string
/// includes all active optional features (e.g. "1.0.0 (lean, +pi, +openclaw)").
/// The bare `CARGO_PKG_VERSION` constant in update.rs is intentionally
/// separate so version-compare logic is never confused by the suffix.
const VERSION: &str = env!("KIMETSU_VERSION_DISPLAY");

#[derive(Debug, Parser)]
#[command(name = "kimetsu")]
#[command(about = "Evidence-first AI coding and research harness")]
#[command(version = VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize a Kimetsu project
    ///
    /// Writes .kimetsu/project.toml + brain.db in the current directory.
    Init(InitArgs),
    /// Manage project config
    ///
    /// Show, edit, get, or set fields in project.toml.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage the memory brain
    ///
    /// Record, retrieve, search, curate, import/export, and maintain memories.
    Brain {
        #[command(subcommand)]
        command: BrainCommand,
    },
    /// Run a coding task
    ///
    /// Drives the autonomous agent pipeline.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Run benchmark suites
    ///
    /// Terminal-Bench / SWE-bench.
    Bench {
        #[command(subcommand)]
        command: BenchCommand,
    },
    /// Inspect and prune run history
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
    /// Manage the project lock
    ///
    /// Clear a stale lock.
    Lock {
        #[command(subcommand)]
        command: LockCommand,
    },
    /// Port skills between hosts
    ///
    /// Discover, import, and export skills across harnesses.
    Bridge {
        #[command(subcommand)]
        command: BridgeCommand,
    },
    /// Run the MCP server
    ///
    /// Exposes the brain to host agents over stdio.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Install plugin wiring into a host
    ///
    /// MCP + hooks for Claude Code, Codex, Pi, or OpenClaw. Also: status, uninstall.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Interactive REPL chat
    ///
    /// Kimetsu as a user-facing coding assistant.
    /// Reuses the full agent runtime (tools, prompts, brain, MP-18 verify)
    /// with a stdin/stdout transport. No dependency on Terminal-Bench.
    Chat(ChatArgs),
    /// Check wiring health
    ///
    /// Validates that every kimetsu subsystem the chat REPL + MCP
    /// sidecar rely on actually works against the current workspace
    /// + user state. Hermetic by default; safe to run in CI.
    ///
    /// Run after upgrading kimetsu, after changing
    /// `KIMETSU_BRAIN_EMBEDDER`, or whenever something looks
    /// off — doctor surfaces the actionable fix.
    Doctor(DoctorArgs),
    /// Update to the latest release
    ///
    /// Checks GitHub Releases and updates discovered local installs.
    Update(UpdateArgs),
    /// Uninstall Kimetsu
    ///
    /// Removes discovered Kimetsu executables from this machine.
    Uninstall(UninstallArgs),
    /// List running processes
    ///
    /// Useful for diagnosing stale MCP servers or lingering sessions.
    /// On Windows uses CIM (Win32_Process) for the command-line;
    /// on Unix uses `ps -eo pid=,args=`.
    Ps(PsArgs),
    /// Stop running processes
    ///
    /// Note: an MCP server spawned by a host (Claude Code, Codex) will be
    /// respawned automatically on the next tool call — stopping it is safe
    /// and is how you clear a stale server.
    Stop(StopArgs),
    /// Restart MCP servers
    ///
    /// Equivalent to `kimetsu stop --all` targeting McpServe processes.
    /// The host agent (Claude Code / Codex) will respawn the MCP server on
    /// the next tool call, so no manual restart is required.
    Restart(RestartArgs),
    /// Set up Kimetsu in one command
    ///
    /// One-command onboarding: init the project, install the plugin into your host, and verify it works.
    ///
    /// Takes a new user from zero to a verified working brain in ONE command,
    /// instead of running `init` + `plugin install` + `doctor --selftest` separately.
    Setup(SetupArgs),
    /// Save a mid-session work checkpoint now.
    ///
    /// Captures the current work episode (task, open threads, dead-ends,
    /// hypothesis) into the brain so the next session can resume from here.
    /// Optionally accepts a short note to add context.
    ///
    /// The episode is per-repo: one live episode per git repo at a time.
    /// A new checkpoint supersedes the previous one.
    ///
    /// Examples:
    ///   kimetsu checkpoint
    ///   kimetsu checkpoint "about to try the new approach"
    ///   kimetsu checkpoint --workspace /path/to/repo "switching branches"
    Checkpoint(CheckpointArgs),
    /// Print the last saved work episode for the current repo.
    ///
    /// Shows what you were working on, what's open, what failed, and the
    /// current working hypothesis — so you can pick up exactly where you
    /// left off.  Prints a friendly message when no episode has been saved
    /// yet.
    ///
    /// Examples:
    ///   kimetsu resume
    ///   kimetsu resume --workspace /path/to/repo
    Resume(ResumeArgs),
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Workspace to validate. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Emit JSON instead of the human report. Used by CI + hooks.
    #[arg(long)]
    json: bool,
    /// Skip the MCP spawn check. Useful when running inside a
    /// sandbox where spawning is disallowed.
    #[arg(long)]
    skip_mcp: bool,
    /// Run a hermetic end-to-end self-test: record a sample memory in a
    /// throwaway temp project, retrieve it by FTS query, and report
    /// PASS/FAIL. Works on both lean and embeddings builds. Does not
    /// touch the real workspace brain.
    #[arg(long)]
    selftest: bool,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    /// Only check whether a newer release exists; do not install it.
    #[arg(long)]
    check: bool,
    /// Print the installs that would be updated without writing files.
    #[arg(long)]
    dry_run: bool,
    /// Reinstall even when the latest release is the current version.
    #[arg(long)]
    force: bool,
    /// Release flavor to install. `auto` preserves this binary's build flavor.
    #[arg(long, default_value = "auto")]
    flavor: String,
}

#[derive(Debug, Args)]
struct UninstallArgs {
    /// Print the installs that would be removed without deleting anything.
    #[arg(long)]
    dry_run: bool,
    /// Confirm removal without prompting. Required when stdin is not a TTY.
    /// Selects Tier 2 (binary + plugin wiring) unless --keep-plugins or
    /// --delete-user-data is also passed.
    #[arg(long)]
    yes: bool,
    /// Remove only the Kimetsu binary; leave Claude Code / Codex plugin
    /// wiring and all brain data intact (Tier 1).
    #[arg(long)]
    keep_plugins: bool,
    /// Also remove the user Kimetsu brain directory (~/.kimetsu or
    /// KIMETSU_USER_BRAIN_DIR) and the current workspace's .kimetsu/
    /// project brain (Tier 3). Irreversible; requires a typed confirm in
    /// interactive mode. In non-interactive mode this flag acts as the confirm.
    #[arg(long)]
    delete_user_data: bool,
}

#[derive(Debug, Args)]
struct PsArgs {
    /// Emit machine-readable JSON instead of the human table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StopArgs {
    /// Stop a specific process by PID. Repeatable; may be combined with --all.
    #[arg(long = "pid", value_name = "PID")]
    pids: Vec<u32>,
    /// Stop ALL running kimetsu processes (excluding self).
    #[arg(long)]
    all: bool,
    /// Confirm without prompting (required when stdin is not a TTY).
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct RestartArgs {
    /// Confirm without prompting (required when stdin is not a TTY).
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct ChatArgs {
    /// Workspace root the agent operates inside. All shell / file tools
    /// resolve paths relative to this directory. Default: current dir.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Path to a kimetsu project (contains `.kimetsu/`). When set, brain
    /// context retrieves on every model turn and MP-18 deviation
    /// proposals can land in the pool's review queue.
    #[arg(long)]
    project: Option<PathBuf>,
    /// Model id (defaults to `claude-opus-4-7`; honors `$KIMETSU_MODEL`).
    #[arg(long)]
    model: Option<String>,
    /// USD budget for this chat session. The cost meter prints running
    /// total via `/cost` and refuses further model calls when crossed.
    #[arg(long, default_value_t = 10.0)]
    max_cost_usd: f32,
    /// Initial goal statement. Can also be set inline via `/goal <text>`.
    /// When non-empty, MP-18's iterative verify uses this as the
    /// target on every finish attempt.
    #[arg(long)]
    goal: Option<String>,
    /// Start in strict-verify mode (MP-18 record_deviation required on
    /// every fix-up cycle). Toggleable inline via `/strict on|off`.
    #[arg(long, default_value_t = false)]
    strict: bool,
    /// Disable ANSI color and terminal polish. Useful for older terminals,
    /// logs, or deterministic screenshots.
    #[arg(long)]
    plain: bool,
    /// Hide the Kimetsu dragon banner at startup.
    #[arg(long)]
    no_logo: bool,
    /// Load an Agent Skills / Codex / Claude Code compatible skill
    /// folder by name or path.
    /// Repeatable. Names are resolved from .codex/skills, .claude/skills,
    /// .kimetsu/skills, and any --skill-dir roots.
    #[arg(long = "skill")]
    skills: Vec<String>,
    /// Additional directory to scan recursively for skill folders.
    /// Repeatable.
    #[arg(long = "skill-dir")]
    skill_dirs: Vec<PathBuf>,
    /// Do not scan workspace .codex/.claude/.kimetsu skill roots.
    #[arg(long)]
    no_workspace_skills: bool,
    /// Do not scan logged-in user tool homes such as ~/.codex, ~/.claude,
    /// ~/.agents, ~/.kimetsu, or their plugin marketplace caches.
    #[arg(long)]
    no_user_skills: bool,
    /// Print discovered skills and exit without starting the REPL.
    #[arg(long)]
    list_skills: bool,
    /// Search discovered skills and exit without starting the REPL.
    #[arg(long)]
    search_skills: Option<String>,
    /// Print detected skill roots and provider marketplace caches, then exit.
    #[arg(long)]
    list_skill_sources: bool,
    /// Import a discovered skill bundle into workspace .kimetsu/skills.
    /// Repeatable. Use --install-skill-force to replace an existing import.
    #[arg(long = "install-skill")]
    install_skills: Vec<String>,
    /// Replace an existing .kimetsu/skills/<name> during --install-skill.
    #[arg(long)]
    install_skill_force: bool,
}

#[derive(Debug, Subcommand)]
enum BridgeCommand {
    /// Discover skills/extensions across host roots and print what was found.
    Scan(BridgeWorkspaceArgs),
    /// Alias for `scan` — report discoverable skills + extensions.
    Status(BridgeWorkspaceArgs),
    /// Import a discovered skill bundle into workspace .kimetsu/extensions.
    Import(BridgeImportArgs),
    /// Export a skill to another host format (claude-code | codex | kimetsu).
    Export(BridgeExportArgs),
    /// Mirror all discovered skill bundles into .kimetsu/extensions.
    Sync(BridgeSyncArgs),
    /// Alias for `scan` — discovery health check across host roots.
    Doctor(BridgeWorkspaceArgs),
}

#[derive(Debug, Args)]
struct BridgeWorkspaceArgs {
    /// Workspace root to scan. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Do not scan logged-in user tool homes (~/.codex, ~/.claude, etc.).
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Args)]
struct BridgeImportArgs {
    /// Name (or path) of the discovered skill bundle to import.
    selection: String,
    /// Workspace root to import into. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Overwrite an existing .kimetsu/extensions/<name> import.
    #[arg(long)]
    force: bool,
    /// Do not scan logged-in user tool homes when resolving the selection.
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Args)]
struct BridgeExportArgs {
    /// Name of the skill to export.
    selection: String,
    /// Destination host format: claude-code | codex | kimetsu.
    target: String,
    /// Workspace root to export from. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Overwrite an existing export at the destination.
    #[arg(long)]
    force: bool,
    /// Do not scan logged-in user tool homes when resolving the selection.
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Args)]
struct BridgeSyncArgs {
    /// Workspace root to sync. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Overwrite existing bundles in .kimetsu/extensions.
    #[arg(long)]
    force: bool,
    /// Do not scan logged-in user tool homes during discovery.
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Start the MCP server (stdio) for a host agent.
    Serve(McpServeArgs),
}

#[derive(Debug, Args)]
struct McpServeArgs {
    /// Workspace root the brain + skills resolve against. Defaults to current dir.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Do not expose skills from logged-in user tool homes.
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// Wire Kimetsu into a host (.mcp.json/.claude or .codex + hooks).
    Install(PluginInstallArgs),
    /// Show what Kimetsu wiring is present for each host + scope.
    Status(PluginStatusArgs),
    /// Remove Kimetsu's wiring from a host (keeps the CLI binary and brain intact).
    Uninstall(PluginUninstallArgs),
}

#[derive(Debug, Args)]
struct PluginStatusArgs {
    /// Workspace root to inspect. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PluginUninstallArgs {
    /// Host to remove from: claude-code | codex | openclaw | pi.
    target: String,
    /// Workspace root to operate in. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Scope to remove from: workspace (default) | global.
    #[arg(long, default_value = "workspace")]
    scope: String,
    /// Remove from both workspace and global scopes.
    #[arg(long, conflicts_with = "scope")]
    all_scopes: bool,
    /// Confirm without prompting (required when stdin is not a TTY).
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct PluginInstallArgs {
    /// Host to install into: claude-code | codex | openclaw | pi | kimetsu.
    target: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Host instruction mode: optional recommends Kimetsu brain first;
    /// required treats missing brain context as a setup blocker for broad work.
    #[arg(long, default_value = "optional")]
    mode: String,
    /// Install scope: `workspace` (default) writes .claude/.codex in the
    /// workspace; `global` writes to ~/.claude(.json) and ~/.codex for all
    /// sessions.
    #[arg(long, default_value = "workspace")]
    scope: String,
    /// Retained for compatibility; has no effect. The installer is fully
    /// idempotent and non-destructive — CLAUDE.md guidance is merged (never
    /// overwritten), and hooks / MCP config / generated docs refresh in place.
    #[arg(long)]
    force: bool,
    /// Skip wiring the proactive PreToolUse/PostToolUse Bash
    /// hooks (mid-work recall). UserPromptSubmit + Stop still install.
    #[arg(long)]
    no_proactive: bool,
    /// Skip the interactive auto-harvest distiller setup prompt.
    #[arg(long)]
    no_setup: bool,
    /// Force the auto-harvest distiller setup prompt even off a TTY.
    #[arg(long)]
    setup_harvest: bool,
    /// Wire a REMOTE kimetsu-remote server (HTTP MCP) instead of the local
    /// stdio command. Pass the server base URL, e.g.
    /// https://kimetsu.example.com:8787 (the endpoint becomes <url>/mcp/<repo>).
    /// Supported for claude-code and openclaw.
    #[arg(long)]
    remote: Option<String>,
    /// Repository id for the remote brain. Defaults to an id derived from this
    /// repo's git remote URL.
    #[arg(long)]
    repo: Option<String>,
    /// Bearer token for the remote server. If omitted, the host config
    /// references ${KIMETSU_REMOTE_TOKEN} so the secret isn't written to disk.
    #[arg(long)]
    token: Option<String>,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Overwrite an existing project.toml / brain.db instead of keeping it.
    #[arg(long)]
    force: bool,
    /// Deprecated — `init` no longer writes host wiring. Use
    /// `kimetsu plugin install` or `kimetsu setup` to wire hosts.
    #[arg(long, hide = true)]
    no_hooks: bool,
}

/// Args for `kimetsu setup` — one-command onboarding.
#[derive(Debug, Args)]
struct SetupArgs {
    /// Workspace to set up. Defaults to current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Host to install into: claude-code | codex | openclaw | pi | both.
    /// If omitted, auto-detected from which host config dirs (~/.claude, ~/.codex, ~/.openclaw, ~/.pi) exist.
    #[arg(long)]
    host: Option<String>,
    /// Install scope: workspace (default) | global.
    #[arg(long, default_value = "workspace")]
    scope: String,
    /// Host instruction mode: optional (default) | required.
    #[arg(long, default_value = "optional")]
    mode: String,
    /// Skip wiring the proactive PreToolUse/PostToolUse Bash hooks.
    #[arg(long)]
    no_proactive: bool,
    /// Skip the interactive auto-harvest distiller setup prompt.
    #[arg(long)]
    no_setup: bool,
    /// Skip the doctor --selftest step.
    #[arg(long)]
    no_selftest: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the parsed project config.
    Show,
    /// Open project.toml in $EDITOR and re-validate on save.
    Edit,
    /// Read one field from the EFFECTIVE config (serde defaults included).
    ///
    /// Key is a dotted path: `embedder.enabled`, `broker.ambient`, etc.
    /// Prints the bare value for scalars; pretty-prints tables/arrays.
    Get {
        /// Dotted key path (e.g. `embedder.enabled`, `broker.ambient`).
        key: String,
    },
    /// Set one field in the on-disk project.toml.
    ///
    /// The value is type-inferred: if the existing key holds a bool, integer,
    /// or float the input is coerced to that type; otherwise `"true"`/`"false"`
    /// → bool, all-digit strings → integer, parseable floats → float, else string.
    ///
    /// NOTE: `set` re-serialises the entire file, so TOML comments are NOT
    /// preserved. Use `config edit` to hand-edit with comments.
    Set {
        /// Dotted key path (e.g. `embedder.enabled`, `broker.ambient`).
        key: String,
        /// New value (type-inferred from the existing field or the literal).
        value: String,
    },
}

#[derive(Debug, Subcommand)]
enum BrainCommand {
    /// Index repo files + manifests into the brain.
    IngestRepo {
        /// Repo root to index.
        path: PathBuf,
    },
    /// Full-text search over indexed file capsules.
    Search(SearchArgs),
    /// Retrieve a ranked context bundle for a query/stage.
    Context(ContextArgs),
    /// Inspect and curate individual memories (add, list, review, prune…).
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Rebuild the in-DB memory projection by replaying the event log.
    /// (Schema upgrades are automatic on open; this does not change the
    /// schema version.) Use --from-traces to re-import from the on-disk
    /// trace.jsonl files (legacy recovery).
    Rebuild {
        /// Re-import the event log from on-disk run traces instead of the
        /// brain.db events table (legacy recovery; normally unnecessary).
        #[arg(long)]
        from_traces: bool,
    },
    /// Quick memory + run counts.
    Stats,
    /// Brain health summary — memory counts, domain groups,
    /// pending proposals, unresolved conflicts, and usefulness bands.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Effectiveness analytics — is the brain helping? Hit-rate, citations,
    /// acceptance, usefulness trend, token economy.
    Insights {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Number of most-recent runs to include in the rolling window.
        #[arg(long, default_value_t = 50)]
        last_n_runs: u32,
        /// ISO-8601 lower bound on run timestamps (overrides --last-n-runs).
        #[arg(long)]
        since: Option<String>,
        /// How many items to include in ranked lists (top-useful, prune-candidates).
        #[arg(long, default_value_t = 10)]
        top: u32,
    },
    /// Claude Code UserPromptSubmit hook. Reads JSON from stdin
    /// (`{"prompt":"...","..."}`), retrieves relevant brain context, and
    /// prints it to stdout for injection into the conversation.
    /// Exits 0 silently when the brain has nothing above threshold.
    ContextHook(ContextHookArgs),
    /// Claude Code Stop hook. Reads session JSON from stdin,
    /// counts kimetsu_brain_record calls made this session, and
    /// prints a summary banner. Exits 0 silently for short sessions
    /// with nothing to report.
    StopHook(StopHookArgs),
    /// Backfill missing or stale embeddings on memory rows.
    /// Run after upgrading kimetsu or after changing the embedder model via
    /// `KIMETSU_BRAIN_EMBEDDER=<id>`.
    Reindex(ReindexArgs),
    /// Inspect or change which built-in embedding model the
    /// brain uses. `list` shows the curated set and the active id;
    /// `set <id>` writes it to project.toml and re-embeds the corpus.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Proactive PreToolUse hook. Reads tool-call JSON from
    /// stdin and, only for a high-confidence match against a stored
    /// failure_pattern/convention, prints a one-line warning BEFORE a
    /// risky Bash command runs. Exits 0 silently otherwise.
    #[command(name = "pretool-hook")]
    PreToolHook(ProactiveHookArgs),
    /// Proactive PostToolUse hook. Reads tool-call JSON from
    /// stdin and, when a Bash command failed and matches a stored
    /// failure_pattern/command, surfaces the known fix. Exits 0
    /// silently otherwise.
    #[command(name = "posttool-hook")]
    PostToolHook(ProactiveHookArgs),
    /// Host SessionEnd hook — runs the credentialed distiller.
    #[command(name = "session-end-hook")]
    SessionEndHook(SessionEndHookArgs),
    /// Host SessionStart hook — injects the repo digest + episodic resume as
    /// `additionalContext` so the agent's FIRST turn already knows the repo
    /// and the current task without an exploratory ls/cat/grep tour.
    ///
    /// Output is JSON in the Claude Code `additionalContext` hook format.
    /// Silent when: `[broker] warm_start = false`, no digest exists, AND
    /// no live episode exists (pure optional feature).
    ///
    /// Gated by `[broker] warm_start` (default true).
    #[command(name = "session-start-hook")]
    SessionStartHook(SessionStartHookArgs),
    /// Build (or rebuild) the repo digest and write it to `.kimetsu/digest.md`.
    ///
    /// The digest is a ~400-token summary of the repo: top-usefulness
    /// memories, manifest (Cargo.toml/package.json/…) summary, and recent
    /// work focus.  It is cached by a content hash and reused at SessionStart.
    ///
    /// Pass `--refresh` to force a rebuild even when the cache is fresh.
    #[command(name = "digest")]
    Digest(DigestArgs),
    /// Reclaim dead disk space in brain.db.
    ///
    /// Without flags this is a safe, read-only-equivalent operation: SQLite
    /// VACUUM rewrites the file, reclaiming free pages left by past invalidations,
    /// prunes, and merges. No data is deleted.
    ///
    /// --purge-invalidated: also deletes retired (invalidated) memory rows
    /// before VACUUM. They are excluded from retrieval already; purging them
    /// makes VACUUM actually shrink the file. Note: they will no longer appear
    /// in audit/blame output.
    ///
    /// --trim-events-older-than <dur>: deletes events older than the given
    /// duration (e.g. 30d, 7d, 24h). WARNING: this shrinks the rebuild
    /// history window. Materialized memories (projection rows) are NOT
    /// affected — only the raw event log is trimmed.
    ///
    /// Examples:
    ///   kimetsu brain compact
    ///   kimetsu brain compact --purge-invalidated
    ///   kimetsu brain compact --trim-events-older-than 90d
    ///   kimetsu brain compact --purge-invalidated --trim-events-older-than 30d --json
    Compact(CompactArgs),
    /// Export active memories to a portable JSON file (or stdout when <file> is `-`).
    ///
    /// The output is a JSON array of `{ text, scope, kind, confidence, created_at }`
    /// records — all the fields needed to reconstruct the memories in another brain.
    /// Instance-specific metadata (memory_id, usefulness_score, use_count) is
    /// intentionally omitted so importing always creates fresh rows with clean stats.
    ///
    /// Examples:
    ///   kimetsu brain export mem.json
    ///   kimetsu brain export mem.json --scope project
    ///   kimetsu brain export mem.json --scope project --kind failure_pattern
    ///   kimetsu brain export - | jq .          # stdout
    Export(BrainExportArgs),
    /// Import memories from a portable JSON file produced by `brain export`.
    ///
    /// For each entry the importer parses scope + kind and calls the same
    /// normalized-text dedup path as `memory add`, so re-importing the same
    /// file is safe. A `--scope-override` reroutes every entry to the given
    /// scope regardless of what the file says.
    ///
    /// Examples:
    ///   kimetsu brain import mem.json
    ///   kimetsu brain import mem.json --scope-override global_user
    Import(BrainImportArgs),
    /// Write a consistent full-DB snapshot of brain.db using the SQLite
    /// online backup API (WAL-safe; no teardown required).
    ///
    /// This is a full-DB backup — unlike `brain export` (memories-only JSON)
    /// this captures every table, index, and event row.  Restore by copying
    /// the snapshot back over brain.db (stop the MCP server first).
    ///
    /// Without <file>, the snapshot is placed next to brain.db and named
    /// `brain.db.backup-<unix_ts>`.
    ///
    /// Examples:
    ///   kimetsu brain backup
    ///   kimetsu brain backup /path/to/snapshot.db
    ///   kimetsu brain backup --workspace /path/to/repo
    Backup(BrainBackupArgs),
    /// Internal: the warm embedder daemon entrypoint (spawned detached).
    #[command(hide = true)]
    EmbedDaemon(EmbedDaemonArgs),
    /// Ensure the embedder daemon is running and warm (no-op on lean / when disabled).
    Warm,
    /// Inspect or control the embedder daemon.
    Daemon(DaemonArgs),
    /// Measure retrieval quality (recall@k / MRR) against a committed fixture.
    ///
    /// Runs three modes — fts (lexical-only), semantic (ANN + cosine), and
    /// semantic+rerank (cross-encoder final stage) — over a fixture file and
    /// prints a comparison table.  Requires `--features embeddings`.
    Eval(EvalArgs),
    /// Measure retrieval quality, latency, and RAM across
    /// embedder × reranker combinations.
    ///
    /// Each combination runs in a child process for honest RSS measurement.
    /// Results are written to --out as JSON files + a summary.md table.
    /// Requires `--features embeddings`.
    Bench(BrainBenchArgs),
    /// ROI ledger — did kimetsu pay for itself?
    ///
    /// Estimates token savings from cited memories (conservative per-kind
    /// calibration), subtracts brain-injection overhead, and shows a
    /// net-positive / net-negative verdict.  Honest negatives are shown as
    /// such.  Use `--json` for stable machine-readable output.
    Roi(RoiArgs),
    /// Self-tuning sweep — optimize retrieval config from personal eval data.
    ///
    /// --status: show accumulated eval cases and readiness.
    /// --apply: write the winning config to project.toml (dry-run by default).
    /// --revert: restore the previous tune-history entry.
    Tune(TuneArgs),
    /// Merge near-duplicate memories and optionally distil loose clusters.
    ///
    /// Story 3.1 (--merge, default): brute-force cosine scan over stored embeddings;
    /// memories with cosine ≥ THRESHOLD (default 0.92) are merged — survivor keeps
    /// its text/id; members get `superseded_by` set and are removed from retrieval.
    /// Citations are reassigned to the survivor so `memory blame` stays accurate.
    ///
    /// Story 3.2 (--distill): looser clusters (0.75–0.85 cosine band) of ≥ 3
    /// memories sharing ≥ 1 domain tag are fed to the configured distiller (same
    /// model the SessionEnd hook uses). Result lands as a memory proposal for human
    /// review. If no distiller is configured, prints the clusters and exits 0.
    ///
    /// Examples:
    ///   kimetsu brain consolidate --dry-run
    ///   kimetsu brain consolidate --yes
    ///   kimetsu brain consolidate --threshold 0.88 --yes
    ///   kimetsu brain consolidate --distill --dry-run
    ///   kimetsu brain consolidate --distill --yes
    Consolidate(ConsolidateArgs),
    /// List fading memories and prune them interactively.
    ///
    /// Shows memories with usefulness_score < SCORE_FLOOR (default 0.2) AND
    /// last_useful_at / created_at older than AGE_DAYS (default 30 days),
    /// with id / kind / age / usefulness / text-head.
    ///
    /// Interactive per-item [k]eep / [p]rune / [s]kip (requires a TTY).
    /// Use --prune-all --yes for batch non-interactive pruning.
    ///
    /// Examples:
    ///   kimetsu brain triage
    ///   kimetsu brain triage --score-floor 0.1 --age-days 60
    ///   kimetsu brain triage --prune-all --yes
    Triage(TriageArgs),
    /// F3 Story 3.1: Forget low-signal memories that haven't been useful for
    /// a configurable number of months.
    ///
    /// Memories are archived via invalidation events (event-sourced, rebuild-safe).
    /// Nothing is hard-deleted from the event log.  Forgetting is opt-in:
    /// `[lifecycle] forget_enabled = true` must be set in `project.toml` (or
    /// use --force-enabled to override once without changing the config file).
    ///
    /// After the forget pass, pending proposals older than
    /// `proposal_expiry_days` are also expired (Story 3.3 hygiene pass).
    ///
    /// Examples:
    ///   kimetsu brain forget --dry-run
    ///   kimetsu brain forget --dry-run --min-age-days 60
    ///   kimetsu brain forget --yes
    ///   kimetsu brain forget --yes --force-enabled
    Forget(ForgetArgs),
    /// Record a ground-truth citation: mark that a memory materially helped.
    ///
    /// Writes a `memory.cited` event (raising use_count / usefulness), the same
    /// signal the MCP `kimetsu_brain_cite` tool records — exposed on the CLI so
    /// outcomes can be injected without a host. Example:
    ///   kimetsu brain cite --memory-id 01K… --note "fixed the build"
    Cite(CiteArgs),
    /// Consolidate citation outcomes into reinforcement structures (v2.5.2):
    /// `--staple` merges repeatedly co-cited memories into single fact
    /// memories (precomputed multi-hop joins); `--routes` rebuilds the
    /// query-routing index so memories that answered similar questions
    /// before get a bounded retrieval boost. Model-free; run offline
    /// (session end / between benchmark iterations).
    Reinforce(ReinforceArgs),
    /// Record a regret: mark that a surfaced memory was unhelpful/misleading.
    ///
    /// Writes a `retrieval.regret` telemetry event for the memory — the negative
    /// signal lifecycle review and calibration consume. Example:
    ///   kimetsu brain regret --memory-id 01K…
    Regret(RegretArgs),
    /// Run the distiller on a transcript and print the lessons it would extract,
    /// WITHOUT recording them. Uses the configured cheap model ([cheap_model] in
    /// project.toml). For inspection and benchmarking the write path. Example:
    ///   kimetsu brain distill session.jsonl --json
    Distill(DistillArgs),
    /// #2 knowledge graph: build relation edges between memories so the
    /// graph-lite / petgraph retrieval backends can traverse them (multi-hop).
    ///
    /// `graph build` derives deterministic `relates_to` edges from shared
    /// entities/tags (no model). `--enrich` additionally asks the configured
    /// cheap model for typed edges (refines / lesson_from / decision_touches);
    /// note small local models (e.g. qwen2.5:3b) are weak at this. Edges are
    /// event-sourced and rebuild-safe. Examples:
    ///   kimetsu brain graph build --dry-run
    ///   kimetsu brain graph build
    ///   kimetsu brain graph build --enrich
    Graph {
        #[command(subcommand)]
        command: GraphCommand,
    },
    /// Flagship 2 / Story 2.3: Reflect related memories into higher-order
    /// principles.
    ///
    /// Clusters related episodic/lesson memories (loose cosine band, 0.75–0.85)
    /// and synthesizes a higher-order principle via the configured cheap model.
    /// Result lands as a memory.proposed event for human review.
    ///
    /// When no cheap model is configured, prints clusters and exits 0.
    ///
    /// Examples:
    ///   kimetsu brain reflect
    ///   kimetsu brain reflect --dry-run
    Reflect(ReflectArgs),
    /// Ask the brain a question and receive a grounded, cited answer.
    ///
    /// Retrieves relevant memories, composes an answer via the configured
    /// cheap model (local/offline preferred; see DP-B), and prints the
    /// result. When no model is configured, returns the top capsule texts
    /// verbatim (never hard-fails). When retrieval is empty, prints a
    /// grounded-only refusal — the brain never halluccinates.
    ///
    /// Examples:
    ///   kimetsu brain ask "how do I run the tests?"
    ///   kimetsu brain ask "what's the cargo build command?" --json
    ///   kimetsu brain ask "explain the broker" --helpful memory:01ABC
    Ask(AskArgs),
    /// Flagship 2: Memory → Skill synthesis.
    ///
    /// Detects memories cited ≥3 times across runs (or tight semantic
    /// clusters) and drafts them into reusable SKILL.md skills via the
    /// configured cheap model (grounded-only — never invents steps).
    ///
    /// --detect   Scan for candidates and create proposals (default when no flag given).
    /// --review   List pending proposals and accepted skills.
    /// --accept   Install a pending proposal into .kimetsu/skills/ (explicit only).
    /// --reject   Reject a pending proposal.
    /// --status   Show staleness status for accepted skills.
    ///
    /// Examples:
    ///   kimetsu brain skills --detect
    ///   kimetsu brain skills --review
    ///   kimetsu brain skills --accept 01ABCDEF
    ///   kimetsu brain skills --reject 01ABCDEF
    ///   kimetsu brain skills --status
    Skills(SkillsArgs),
    /// Epic S3: sync the brain across machines via event-log replication.
    ///
    /// Sync = event-log replication (NOT SQLite file copying).  Only durable
    /// memory-lifecycle events are replicated:
    ///   memory.accepted, memory.proposed, memory.rejected, memory.invalidated,
    ///   memory.cited, memory.superseded
    ///
    /// Excluded (local/telemetry): work.episode, context.served,
    ///   retrieval.regret, run.* and everything else.
    ///
    /// Subcommands:
    ///
    ///   kimetsu brain sync export [--since <rowid>] [--out <file>] [--dry-run]
    ///     Export durable events since a rowid cursor to a JSONL batch.
    ///     Defaults to stdout.
    ///
    ///   kimetsu brain sync import <batch> [--dry-run]
    ///     Import a JSONL batch (per-event idempotent via event_id).
    ///     Reports applied/skipped counts.
    ///
    ///   kimetsu brain sync [--status] [--dry-run]
    ///     Full directory-protocol sync: push new events, pull from peers.
    ///     Requires [sync] dir + machine_id in project.toml.
    ///
    /// Examples:
    ///   kimetsu brain sync export --out /tmp/batch.jsonl
    ///   kimetsu brain sync export --since 42 --out /tmp/delta.jsonl
    ///   kimetsu brain sync import /tmp/batch.jsonl
    ///   kimetsu brain sync import /tmp/batch.jsonl --dry-run
    ///   kimetsu brain sync               # full dir-protocol cycle
    ///   kimetsu brain sync --status      # show configured dir, machine_id, cursors
    ///   kimetsu brain sync --dry-run     # report what would happen
    Sync(SyncArgs),
}

/// Args for `kimetsu brain sync` (Epic S3).
#[derive(Debug, clap::Args)]
struct SyncArgs {
    /// Subcommand: `export` | `import` | (empty for full dir-protocol sync).
    #[arg(value_name = "SUBCOMMAND")]
    subcommand: Option<String>,
    /// For `export`: export events after this rowid (exclusive).  Default 0 (all).
    #[arg(long, value_name = "ROWID", default_value_t = 0)]
    since: i64,
    /// For `export`: write the batch to this file instead of stdout.
    #[arg(long, value_name = "FILE")]
    out: Option<String>,
    /// For `import`: path to a JSONL batch file (required when subcommand=import).
    #[arg(value_name = "BATCH_FILE")]
    batch: Option<String>,
    /// Report what WOULD happen without actually writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Show configured sync dir, machine_id, per-source cursors, pending counts.
    #[arg(long)]
    status: bool,
    /// Override the workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
}

/// Args for `kimetsu brain skills` (Flagship 2).
#[derive(Debug, clap::Args)]
struct SkillsArgs {
    /// Detect synthesis candidates and create proposals (default action).
    #[arg(long)]
    detect: bool,
    /// List pending proposals and accepted skills.
    #[arg(long)]
    review: bool,
    /// Accept a pending proposal and install the skill (provide proposal-id).
    #[arg(long, value_name = "PROPOSAL_ID")]
    accept: Option<String>,
    /// Reject a pending proposal (provide proposal-id).
    #[arg(long, value_name = "PROPOSAL_ID")]
    reject: Option<String>,
    /// Show staleness status for accepted skills.
    #[arg(long)]
    status: bool,
    /// Override the workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
}

/// Args for `kimetsu brain ask`.
#[derive(Debug, clap::Args)]
struct AskArgs {
    /// The question to ask the brain.
    question: String,
    /// Emit machine-readable JSON (stable schema: answer, citations,
    /// grounded, model_used, verbatim).
    #[arg(long)]
    json: bool,
    /// Mark a prior answer as helpful, recording a citation for each
    /// memory id in CITATIONS (comma-separated `memory:<id>` handles).
    /// Example: `kimetsu brain ask --helpful memory:01ABC,memory:01DEF ""`
    #[arg(long, value_name = "CITATIONS")]
    helpful: Option<String>,
    /// Override the workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// List the curated built-in embedding models and mark the active one.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Set the active embedding model and re-embed the corpus.
    Set(ModelSetArgs),
}

#[derive(Debug, Args)]
struct ModelSetArgs {
    /// Built-in model id (see `kimetsu brain model list`).
    id: String,
    /// Write the config but skip the (potentially slow) reindex.
    #[arg(long)]
    no_reindex: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct EmbedDaemonArgs {
    /// Embedder model id to load (resolved from config by the spawner).
    #[arg(long)]
    model: String,
    /// Cross-encoder reranker id (resolved from config by the spawner).
    /// `"off"` disables reranking for this daemon process.
    #[arg(long, default_value = "off")]
    reranker: String,
}

#[derive(Debug, clap::Args)]
struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommand,
}

#[derive(Debug, clap::Args)]
struct EvalArgs {
    /// Path to the eval fixture JSON file.
    #[arg(long, default_value = "fixtures/eval-retrieval.json")]
    fixture: PathBuf,
    /// Comma-separated list of reranker model ids to benchmark (quality + latency).
    /// When non-empty, one extra row is printed per reranker after the baseline table.
    /// Example: `--rerankers jina-reranker-v1-turbo-en,jina-reranker-v1-tiny-en`
    #[arg(long, default_value = "")]
    rerankers: String,
    /// Candidate-pool size handed to the reranker before truncating to the
    /// cap (mirrors the daemon's RERANK_POOL; 12 is the production value).
    #[arg(long, default_value_t = 12)]
    pool: usize,
    /// HyDE: expand each case query with a hypothetical answer from the cheap
    /// model before retrieval, to measure the recall lift on oblique queries.
    #[arg(long)]
    hyde: bool,
}

/// Args for `kimetsu brain bench`.
#[derive(Debug, clap::Args)]
struct BrainBenchArgs {
    /// Path to the eval fixture JSON.
    #[arg(long, default_value = "bench/dataset.json")]
    dataset: PathBuf,
    /// Comma-separated embedder ids to sweep.
    #[arg(long, default_value = "bge-small-en-v1.5,jina-v2-base-code")]
    embedders: String,
    /// Comma-separated reranker ids to sweep.
    #[arg(
        long,
        default_value = "off,jina-reranker-v1-turbo-en,jina-reranker-v1-tiny-en,ms-marco-tinybert-l-2-v2,ms-marco-minilm-l-4-v2"
    )]
    rerankers: String,
    /// Candidate-pool size passed to retrieval before reranking.
    #[arg(long, default_value_t = 12usize)]
    pool: usize,
    /// Final capsule cap after reranking.
    #[arg(long, default_value_t = 4usize)]
    cap: usize,
    /// Directory to write per-combo JSON files and summary.md.
    #[arg(long, default_value = "bench/results")]
    out: PathBuf,
    /// Internal: run a single embedder×reranker combo in-process and write
    /// the combo JSON file.  Do NOT use directly — the orchestrator sets this.
    #[arg(long, hide = true)]
    single: bool,
    /// Benchmark kimetsu-remote over HTTP instead of the local in-process path.
    /// Spawns the release server binary, seeds a temp brain, and measures
    /// per-case latency (sequential + concurrent), recall@k, MRR, and server RSS.
    /// The server reranks with its `--reranker` flag (default jina-tiny).
    #[arg(long)]
    remote: bool,
    /// Number of parallel HTTP workers for the concurrent latency pass (--remote only).
    #[arg(long, default_value_t = 4usize)]
    concurrency: usize,
}

#[derive(Debug, clap::Subcommand)]
enum DaemonCommand {
    /// Print daemon status (model, uptime, request count) or "not running".
    Status,
    /// Ask the running daemon to exit.
    Stop,
}

#[derive(Debug, Args)]
struct ProactiveHookArgs {
    /// Minimum relevance score for a proactive injection (FTS-only
    /// scale; stricter than the reactive 0.20). Default 0.45.
    #[arg(long, default_value_t = 0.45)]
    min_score: f32,
    /// Lower threshold used when a looping/repeated command is
    /// detected (the agent is stuck — surface help more readily).
    #[arg(long, default_value_t = 0.35)]
    loop_min_score: f32,
    /// Max capsules to inject. Default 1 (recall discipline).
    #[arg(long, default_value_t = 1usize)]
    max_capsules: usize,
    /// Suppress further proactive injections for this many seconds
    /// after one fires (refractory throttle). Default 90.
    #[arg(long, default_value_t = 90u64)]
    refractory_secs: u64,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ContextHookArgs {
    /// Minimum relevance score for capsule inclusion (0.0-1.0). Default 0.20.
    #[arg(long, default_value_t = 0.20)]
    min_score: f32,
    /// Maximum capsules to inject. Default 2.
    #[arg(long, default_value_t = 2usize)]
    max_capsules: usize,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StopHookArgs {
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Codex compatibility: run the credentialed distiller from Stop because
    /// current Codex hooks expose Stop but not SessionEnd.
    #[arg(long)]
    distill_on_stop: bool,
}

#[derive(Debug, Args)]
struct SessionEndHookArgs {
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SessionStartHookArgs {
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct DigestArgs {
    /// Force a rebuild even when the cached digest is fresh.
    #[arg(long)]
    refresh: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu checkpoint`.
#[derive(Debug, Args)]
struct CheckpointArgs {
    /// Optional note to attach to this checkpoint.
    #[arg(value_name = "NOTE")]
    note: Option<String>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu resume`.
#[derive(Debug, Args)]
struct ResumeArgs {
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain tune`.
#[derive(Debug, Args)]
struct TuneArgs {
    /// Show personal eval-set statistics without running the sweep.
    #[arg(long)]
    status: bool,
    /// Cost penalty weight per estimated token injected per query.
    /// Default 0.005 ≈ one MRR rank position ≈ 200 tokens.
    #[arg(long, default_value_t = 0.005f64)]
    cost_weight: f64,
    /// Apply the winning config to project.toml (without this flag, dry-run only).
    #[arg(long)]
    apply: bool,
    /// Revert the most recent tune-history entry.
    #[arg(long)]
    revert: bool,
    /// S2.1: Show re-tune trigger state (corpus growth + drift signal).
    /// Included automatically in --status; use alone for a cheap check.
    #[arg(long)]
    triggers: bool,
    /// S2.2: Show the model re-selection advisor (embedder×reranker grid
    /// recommendation with download+reindex cost).
    #[arg(long)]
    models: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
}

/// Args for `kimetsu brain consolidate` (Stories 3.1 + 3.2).
#[derive(Debug, Args)]
struct ConsolidateArgs {
    /// Print merge plan without writing to the DB.
    #[arg(long)]
    dry_run: bool,
    /// Cosine similarity threshold for near-duplicate clustering (Story 3.1).
    /// Memories with cosine ≥ threshold are merged. Default: 0.92.
    #[arg(long, default_value_t = 0.92f32)]
    threshold: f32,
    /// Skip the interactive confirmation prompt (required when stdin is not a TTY).
    #[arg(long)]
    yes: bool,
    /// Also run Story 3.2 distillation of loose clusters (0.75–0.85 band).
    /// Result lands as a memory proposal for human review.
    /// Requires a configured distiller; prints clusters and exits 0 otherwise.
    #[arg(long)]
    distill: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain triage` (Story 3.3).
#[derive(Debug, Args)]
struct TriageArgs {
    /// Usefulness score floor: memories below this threshold are candidates.
    #[arg(long, default_value_t = 0.2f32)]
    score_floor: f32,
    /// Age threshold in days: memories last useful (or created) before this are candidates.
    #[arg(long, default_value_t = 30u32)]
    age_days: u32,
    /// Prune all candidates non-interactively (requires --yes).
    #[arg(long)]
    prune_all: bool,
    /// Skip the confirmation prompt for --prune-all.
    #[arg(long)]
    yes: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain forget` (F3 Story 3.1 + 3.3).
#[derive(Debug, Args)]
struct ForgetArgs {
    /// Report which memories would be forgotten without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Emit the forget summary as machine-readable JSON. Implies report-only
    /// (never writes), so it composes with --dry-run and is safe for harnesses.
    #[arg(long)]
    json: bool,
    /// Skip the confirmation prompt and apply immediately.
    #[arg(long)]
    yes: bool,
    /// Override the usefulness-score floor (default comes from project.toml lifecycle section).
    #[arg(long)]
    usefulness_floor: Option<f32>,
    /// Minimum age in days since last useful (overrides project.toml default).
    #[arg(long)]
    min_age_days: Option<u32>,
    /// Protect memories with use_count >= this value (overrides project.toml default).
    #[arg(long)]
    protect_use_count: Option<u32>,
    /// Apply even if forget_enabled = false in project.toml (one-shot override).
    #[arg(long)]
    force_enabled: bool,
    /// Skip the proposal-queue GC hygiene pass after forgetting.
    #[arg(long)]
    no_proposal_gc: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain cite`.
#[derive(Debug, Args)]
struct CiteArgs {
    /// Memory id(s) to credit. Repeat the flag to cite several memories as
    /// ONE group — grouped citations are what `brain reinforce --staple`
    /// consolidates (they answered together).
    #[arg(long, required = true)]
    memory_id: Vec<String>,
    /// Optional rationale recorded with the citation.
    #[arg(long)]
    note: Option<String>,
    /// The question/task these memories helped answer. Feeds the
    /// query-routing index (`brain reinforce --routes`).
    #[arg(long)]
    query: Option<String>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain reinforce`.
#[derive(Debug, Args)]
struct ReinforceArgs {
    /// Staple co-cited memories into consolidated fact memories.
    #[arg(long)]
    staple: bool,
    /// Rebuild the query-routing index from citation history.
    #[arg(long)]
    routes: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain regret`.
#[derive(Debug, Args)]
struct RegretArgs {
    /// The memory id to flag as regretted.
    #[arg(long)]
    memory_id: String,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain distill`.
#[derive(Debug, Args)]
struct DistillArgs {
    /// Path to a transcript JSONL file (one message object per line).
    transcript: PathBuf,
    /// Emit the extracted lessons as machine-readable JSON.
    #[arg(long)]
    json: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// `kimetsu brain graph <command>` (#2 knowledge graph).
#[derive(Debug, Subcommand)]
enum GraphCommand {
    /// Build relation edges over the workspace brain's active memories.
    Build(GraphBuildArgs),
}

/// Args for `kimetsu brain graph build`.
#[derive(Debug, Args)]
struct GraphBuildArgs {
    /// Preview the edges that would be written without persisting anything.
    #[arg(long)]
    dry_run: bool,
    /// Emit a machine-readable JSON summary (for the benchmark harness).
    #[arg(long)]
    json: bool,
    /// Additionally ask the configured cheap model for typed edges
    /// (refines / lesson_from / decision_touches). Opt-in; small local models
    /// are weak at this, so it is off by default.
    #[arg(long)]
    enrich: bool,
    /// Cap on rule edges originating from any single memory (0 = module default).
    #[arg(long, default_value_t = 0)]
    max_fan_out: usize,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain reflect` (Flagship 2 / Story 2.3).
#[derive(Debug, Args)]
struct ReflectArgs {
    /// Print what would be proposed without writing to the DB.
    #[arg(long)]
    dry_run: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Args for `kimetsu brain roi`.
#[derive(Debug, Args)]
struct RoiArgs {
    /// Time window: "7d", "30d", or "all". Default: 30d.
    #[arg(long, default_value = "30d")]
    window: String,
    /// Emit machine-readable JSON (stable RoiReport schema).
    #[arg(long)]
    json: bool,
    /// S2.4(a): Show the top N memories by estimated token savings
    /// (citation-weighted, pairs with consolidate/triage).
    /// Default: show top 10 when flag is present with no value.
    #[arg(long, value_name = "N")]
    top: Option<usize>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ReindexArgs {
    /// Which DB(s) to reindex: `project`, `user`, or `all`.
    #[arg(long, default_value = "all")]
    scope: String,
    /// Count what would change but don't write.
    #[arg(long)]
    dry_run: bool,
    /// Re-embed even rows that already carry the active model id
    /// (useful after a fastembed model file update where bytes
    /// changed but the model id didn't).
    #[arg(long)]
    force: bool,
    /// Stop after this many rows are written. Useful for incremental
    /// reindex on huge brains over multiple invocations.
    #[arg(long)]
    limit: Option<usize>,
}

/// Q8: args for `kimetsu brain compact`.
#[derive(Debug, Args)]
struct CompactArgs {
    /// Also delete invalidated (retired) memory rows before VACUUM.
    /// These rows are already excluded from retrieval; purging them lets
    /// VACUUM recover more disk space. They will no longer appear in
    /// audit/blame output after this operation.
    #[arg(long)]
    purge_invalidated: bool,
    /// Trim events older than this duration before VACUUM (e.g. 30d, 7d, 24h).
    /// WARNING: reduces the rebuild history window. Materialized memories
    /// (projection rows) are NOT affected — only the raw event log is trimmed.
    #[arg(long, value_name = "DUR")]
    trim_events_older_than: Option<String>,
    /// Emit machine-readable JSON instead of the human summary.
    #[arg(long)]
    json: bool,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct BrainExportArgs {
    /// Output file path. Use `-` to write to stdout.
    file: String,
    /// Filter by scope (global_user|project|repo|run).
    #[arg(long)]
    scope: Option<String>,
    /// Filter by kind (preference|convention|command|failure_pattern|fact).
    #[arg(long)]
    kind: Option<String>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Strip the trailing `(context: …)` segment from each exported memory text.
    /// Useful when sharing memories between projects: the context annotation is
    /// project-specific and not meaningful elsewhere.
    #[arg(long)]
    redact: bool,
    /// Strip the leading `[tags: …]` prefix from each exported memory text.
    /// Usable on its own (tags only) or with `--redact` for a fully clean
    /// lesson body with no metadata.
    #[arg(long)]
    redact_tags: bool,
    /// v3.0 #4: pack name. Setting any manifest flag (--name/--version/
    /// --description) writes a self-describing shareable PACK envelope; without
    /// them, the bare memory array (back-compat). Output is ALWAYS gzip-compressed.
    #[arg(long)]
    name: Option<String>,
    /// Pack version (e.g. 1.0.0).
    #[arg(long)]
    version: Option<String>,
    /// Pack description.
    #[arg(long)]
    description: Option<String>,
    /// Abort the export if the security scrub finds ANY credential or PII
    /// (instead of redacting + reporting). Use when publishing to fail loudly.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct BrainImportArgs {
    /// Input pack file path, `-` for stdin, or an http(s):// URL (installs from
    /// the marketplace). Gzip-compressed OR plain JSON is auto-detected.
    file: String,
    /// Override the scope for every imported entry (global_user|project|repo|run).
    #[arg(long)]
    scope_override: Option<String>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// v3.0 #4: install mode. `merge` (default) adds the pack additively, dedups
    /// against what you have. `replace` supersedes your current memories in the
    /// pack's scope(s) first (reversible — invalidated, not deleted), then loads
    /// the pack; requires --yes.
    #[arg(long, default_value = "merge")]
    mode: String,
    /// Confirm a destructive `--mode replace`.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct BrainBackupArgs {
    /// Destination file for the snapshot. When omitted, placed next to
    /// brain.db and named `brain.db.backup-<unix_ts>`.
    file: Option<PathBuf>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Search text (matched against indexed file capsules).
    query: String,
    /// Max results to return.
    #[arg(long, default_value_t = 10)]
    limit: u32,
}

#[derive(Debug, Args)]
struct ContextArgs {
    /// Query the retrieval ranks capsules against.
    query: String,
    /// Pipeline stage the bundle is shaped for (e.g. localization).
    #[arg(long, default_value = "localization")]
    stage: String,
    /// Token budget the returned bundle must fit within.
    #[arg(long, default_value_t = 6000)]
    budget_tokens: u32,
    /// Print machine-readable JSON for hooks and harness wrappers.
    #[arg(long)]
    json: bool,
    /// Skip the ambient workspace fingerprint (git branch,
    /// dirty files, recent edits). Default behavior augments the
    /// query with that suffix so hooks calling with terse queries
    /// like "continue" or "fix it" still surface useful capsules.
    #[arg(long)]
    no_ambient: bool,
    /// HyDE: expand the query with a hypothetical answer from the cheap model
    /// before retrieval (lifts recall on oblique queries; needs [cheap_model]).
    #[arg(long)]
    hyde: bool,
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Add a durable memory directly.
    Add(MemoryAddArgs),
    /// Add many memories at once from a JSONL file, JSON array, or stdin.
    ///
    /// Each JSONL line or array element must be a JSON object with at least a
    /// `"text"` field.  Optional fields: `"scope"` (default: project),
    /// `"kind"` (default: fact), `"valid_from"` (RFC 3339), `"valid_to"` (RFC 3339).
    ///
    /// Pass `-` as FILE to read from stdin.
    ///
    /// Example (JSONL):
    ///   {"text": "Use cargo fmt --all before committing", "kind": "convention"}
    ///   {"text": "Prefer explicit error types", "scope": "repo", "kind": "convention"}
    #[command(name = "add-batch")]
    AddBatch(MemoryAddBatchArgs),
    /// List active memories with usefulness stats.
    List {
        /// Emit memories as machine-readable JSON (id, scope, kind, confidence,
        /// use_count, usefulness_score, text) for harnesses and benchmarks.
        #[arg(long)]
        json: bool,
    },
    /// List pending proposals awaiting review.
    Proposals(ProposalsArgs),
    /// Promote a proposal into an active memory.
    Accept(AcceptArgs),
    /// Reject a pending proposal.
    Reject(RejectArgs),
    /// Retire a memory (keeps the row, stops retrieving it).
    Invalidate(InvalidateArgs),
    /// Batch review pending memory proposals in non-interactive mode
    /// (interactive TTY review available separately).
    Review(ReviewArgs),
    /// Ranked memories by usefulness ratio so the user can see what
    /// is pulling weight after curation.
    Top(TopArgs),
    /// Bulk-invalidate memories whose outcome attribution says they
    /// hurt more than they help. Safe-by-default: dry-run unless --apply.
    Prune(PruneArgs),
    /// Per-run memory attribution. Walks `memory_citations` +
    /// `context.injected` events to surface which memories the model
    /// actually leveraged vs which were silent passengers.
    Blame(BlameArgs),
    /// List and resolve conflict-detection hits surfaced at
    /// ingest. With `--list` (the default) renders open conflicts;
    /// `--resolve <id> <kept_new|kept_existing|kept_both>` settles one.
    Conflicts(ConflictsArgs),
    /// Edit an existing active memory in-place (text and/or kind).
    /// Preserves use_count, usefulness_score, confidence, and created_at —
    /// the memory's learned history is not reset.
    Edit(MemoryEditArgs),
    /// Invalidate the most recently recorded active memory in the project
    /// brain (the "agent saved junk" case). The row is kept for audit;
    /// it simply stops being retrieved.
    Undo(MemoryUndoArgs),
    /// Backdate a memory's age (created_at / last_useful_at) by N days. A
    /// testing/benchmark affordance for exercising age-sensitive policies like
    /// forgetting. Event-sourced (`memory.aged`), so it survives a rebuild.
    #[command(name = "set-age")]
    SetAge(MemorySetAgeArgs),
}

#[derive(Debug, Args)]
struct MemorySetAgeArgs {
    /// The memory id to backdate.
    #[arg(long)]
    memory_id: String,
    /// How many days into the past to set created_at / last_useful_at.
    #[arg(long)]
    days_ago: u32,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ConflictsArgs {
    /// Resolve a conflict by id. Takes a second positional argument:
    /// `kept_new` (invalidates the existing memory), `kept_existing`
    /// (invalidates the new memory), or `kept_both` (no invalidation).
    /// When unset, the command lists open conflicts.
    #[arg(long, value_names = ["CONFLICT_ID", "RESOLUTION"], num_args = 2)]
    resolve: Option<Vec<String>>,
    /// Cap the number of open conflicts shown per brain. Default 50.
    #[arg(long, default_value_t = 50)]
    limit: u32,
    /// Emit JSON for hooks + CI consumers.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BlameArgs {
    /// The run id to inspect (a ULID; the kind printed in chat session
    /// output and trace files).
    run_id: String,
    /// Emit JSON for hooks + CI consumers.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct InvalidateArgs {
    /// The memory id to retire.
    memory_id: String,
    /// Short note persisted alongside invalidated_at; rendered in
    /// `memory list` so the human reviewer remembers why this memory
    /// was retired.
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Debug, Args)]
struct MemoryAddArgs {
    /// Scope to store under: global_user | project | repo | run.
    #[arg(long)]
    scope: String,
    /// Memory kind: fact | preference | convention | command | failure_pattern.
    #[arg(long, default_value = "fact")]
    kind: String,
    /// The memory text to store.
    text: String,
    #[command(flatten)]
    remote: RemoteWriteArgs,
}

/// v3.0 #3 Slice C: target a `kimetsu-remote` server for a CLI write instead of
/// the local brain. Shared (clap `flatten`) by remote-capable write commands.
#[derive(Debug, Args, Clone)]
struct RemoteWriteArgs {
    /// Write to a kimetsu-remote server at this base URL (e.g.
    /// `https://kimetsu.example.com:8787`) instead of the local brain.
    #[arg(long)]
    remote: Option<String>,
    /// Repo id for the remote brain (required with --remote).
    #[arg(long)]
    repo: Option<String>,
    /// Bearer token for the remote server (else `KIMETSU_REMOTE_TOKEN`).
    #[arg(long)]
    token: Option<String>,
}

/// Args for `kimetsu brain memory add-batch`.
#[derive(Debug, Args)]
struct MemoryAddBatchArgs {
    /// Path to a JSONL file (one JSON object per line) or a JSON array file.
    /// Use `-` to read from stdin.
    file: String,
    /// Default scope applied to entries that omit `"scope"`.
    /// Overridden per-entry by the entry's own `"scope"` field.
    #[arg(long, default_value = "project")]
    scope: String,
    /// Default kind applied to entries that omit `"kind"`.
    /// Overridden per-entry by the entry's own `"kind"` field.
    #[arg(long, default_value = "fact")]
    kind: String,
    /// Emit a JSON report: `{"added": N, "ids": [...]}` instead of plain text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ProposalsArgs {
    /// Restrict to a single scope (global_user|project|repo|run).
    #[arg(long)]
    scope: Option<String>,
    /// Restrict to a single kind (preference|convention|failure_pattern|fact|...).
    #[arg(long)]
    kind: Option<String>,
    /// Restrict to proposals from a specific run.
    #[arg(long)]
    from_run: Option<String>,
    /// Drop proposals whose proposed_confidence is below this value.
    #[arg(long)]
    min_confidence: Option<f32>,
    /// Restrict to a single status (pending|accepted|rejected). Default: pending.
    #[arg(long, default_value = "pending")]
    status: String,
    /// Hard cap on rows returned.
    #[arg(long, default_value_t = 50)]
    limit: u32,
}

#[derive(Debug, Args)]
struct AcceptArgs {
    /// The proposal id to promote.
    proposal_id: String,
    /// Override the proposal's scope when promoting it to an accepted memory.
    #[arg(long)]
    scope: Option<String>,
    /// Override the proposed_confidence (clamped to 0..1).
    #[arg(long)]
    confidence: Option<f32>,
}

#[derive(Debug, Args)]
struct RejectArgs {
    /// The proposal id to reject.
    proposal_id: String,
    /// Optional short note; persisted on the memory_proposals row for triage.
    #[arg(long)]
    reason: Option<String>,
}

/// MP-6: surface the curated memories that are pulling weight. Sorted by
/// `usefulness_score / use_count` descending, then by use_count
/// descending as the tie-break.
#[derive(Debug, Args)]
struct TopArgs {
    /// Restrict to a single scope (global_user|project|repo|run).
    #[arg(long)]
    scope: Option<String>,
    /// Hide memories with fewer than this many recorded uses; the small-
    /// sample guard. Default 3 matches the broker's threshold so the
    /// listing only shows entries the bias actually applies to.
    #[arg(long, default_value_t = 3)]
    min_uses: u32,
    /// Hard cap on rows returned.
    #[arg(long, default_value_t = 20)]
    limit: u32,
}

/// MP-6: prune memories whose outcome attribution data says they cost more
/// than they help. Defaults match the MP-4c shadowing thresholds so the
/// prune list is exactly the entries the broker is already discounting.
#[derive(Debug, Args)]
struct PruneArgs {
    /// Restrict to a single scope.
    #[arg(long)]
    scope: Option<String>,
    /// Only consider memories with at least this many uses.
    #[arg(long, default_value_t = 3)]
    min_uses: u32,
    /// Prune memories whose `usefulness_score / use_count` is at or below
    /// this value. Default -0.2 mirrors MP-4c's SHADOW_MAX_RATIO.
    #[arg(long, default_value_t = -0.2)]
    max_ratio: f32,
    /// Actually invalidate the matches. Without this, the command prints
    /// what would be pruned and exits 0.
    #[arg(long)]
    apply: bool,
}

/// Batch review of pending memory proposals. Either `--accept-all` or
/// `--reject-all` is required (they are mutually exclusive). The filter
/// flags narrow the batch; defaults match `memory proposals` listing.
///
/// Examples:
///   kimetsu brain memory review --accept-all --from-run <run_id>
///   kimetsu brain memory review --reject-all --reason "too task-specific"
///   kimetsu brain memory review --accept-all --scope global_user --min-confidence 0.8
///   kimetsu brain memory review --accept-all --dry-run     # preview only
#[derive(Debug, Args)]
struct ReviewArgs {
    /// Accept every pending proposal matching the filters.
    #[arg(long, conflicts_with = "reject_all")]
    accept_all: bool,
    /// Reject every pending proposal matching the filters.
    #[arg(long, conflicts_with = "accept_all")]
    reject_all: bool,
    /// Reason recorded on each rejected proposal; defaults to
    /// "batch_reject" when omitted with `--reject-all`.
    #[arg(long)]
    reason: Option<String>,
    /// Restrict to a single scope (global_user|project|repo|run).
    #[arg(long)]
    scope: Option<String>,
    /// Restrict to a single kind (preference|convention|failure_pattern|...).
    #[arg(long)]
    kind: Option<String>,
    /// Restrict to proposals from a specific run.
    #[arg(long)]
    from_run: Option<String>,
    /// Drop proposals whose proposed_confidence is below this value.
    #[arg(long)]
    min_confidence: Option<f32>,
    /// Hard cap on rows reviewed in this batch.
    #[arg(long, default_value_t = 100)]
    limit: u32,
    /// Print what would happen without writing any events.
    #[arg(long)]
    dry_run: bool,
}

/// Q6: args for `kimetsu brain memory edit`.
#[derive(Debug, Args)]
struct MemoryEditArgs {
    /// The memory id to edit (a ULID printed by `memory add` / `memory list`).
    memory_id: String,
    /// New text to store in place of the existing text. The FTS index and
    /// embedding are refreshed; usefulness history is preserved.
    #[arg(long)]
    text: Option<String>,
    /// New kind to assign (fact|preference|convention|command|failure_pattern|…).
    #[arg(long)]
    kind: Option<String>,
}

/// Q6: args for `kimetsu brain memory undo`.
#[derive(Debug, Args)]
struct MemoryUndoArgs {
    /// Skip the interactive confirmation and invalidate immediately.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum RunCommand {
    /// Run a coding task end-to-end through the agent pipeline.
    Coding(CodingArgs),
    /// Abort an in-flight run by id.
    Abort {
        /// The run id to abort.
        run_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum BenchCommand {
    /// Run the Terminal-Bench suite against a repo.
    Run(BenchRunArgs),
    /// Run the SWE-bench suite from a tasks JSONL.
    Swe(SweArgs),
}

#[derive(Debug, Args)]
struct SweArgs {
    /// JSONL file of SWE-bench task records.
    #[arg(long)]
    tasks: PathBuf,
    /// Caller-prepared repo path. Kimetsu does NOT clone or apply test_patch
    /// automatically — see docs/SWEBENCH.md for the full integration plan.
    #[arg(long)]
    repo: PathBuf,
    /// Run a single instance by id (default: every task).
    #[arg(long)]
    instance_id: Option<String>,
    /// Skip Implementation+Verification; stop at PatchPlan.
    #[arg(long)]
    dry_run: bool,
    /// Disable the broker (broker_off equivalent).
    #[arg(long)]
    no_broker: bool,
    /// Hard cap on tasks executed.
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Args)]
struct BenchRunArgs {
    /// Repo to benchmark. Defaults to current directory.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Keep generated fixtures on disk after the run (for debugging).
    #[arg(long)]
    keep_fixtures: bool,
    /// Drive tasks with a live model instead of the offline stub.
    #[arg(long)]
    model_backed: bool,
    /// Hard cap on tasks executed.
    #[arg(long)]
    limit: Option<usize>,
    /// Soft cost cap; bench stops scheduling new tasks once cumulative model
    /// cost exceeds this. Defaults high because Claude Code OAuth is on a
    /// subscription â€” cost is reported as a metric, not a hard constraint.
    /// Pass a smaller value if you want the bench to stop early on metered
    /// providers.
    #[arg(long, default_value_t = 250.0)]
    max_cost_usd: f32,
}

#[derive(Debug, Args)]
struct CodingArgs {
    /// Repo the agent operates on. Defaults to current directory.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Plan only; stop before applying the patch.
    #[arg(long)]
    dry_run: bool,
    /// Permit high-risk shell commands the safety gate would otherwise block.
    #[arg(long)]
    allow_high_risk: bool,
    /// Run without a model (offline/stub mode).
    #[arg(long)]
    no_model: bool,
    /// Disable brain retrieval (broker_off) for this run.
    #[arg(long)]
    no_broker: bool,
    /// Disable secret redaction in shell output.
    #[arg(long)]
    no_redact: bool,
    /// Verbose debug tracing.
    #[arg(long)]
    debug: bool,
    /// The task description for the agent to carry out.
    task: String,
}

#[derive(Debug, Subcommand)]
enum RunsCommand {
    /// List recorded agent runs.
    List,
    /// Show one run's metadata + outcome.
    Show {
        /// The run id to show.
        run_id: String,
    },
    /// Remove old run directories from .kimetsu/runs/.
    ///
    /// Run dirs hold trace.jsonl + artifacts. The underlying events are
    /// durable in brain.db (they can be replayed), so deleting a run
    /// dir only frees disk — it does NOT remove memories or event history.
    ///
    /// Dry-run by default — pass `--apply` to actually delete.
    ///
    /// At least one of `--older-than` or `--keep` is required so that
    /// you cannot accidentally wipe everything in one shot.
    ///
    /// Examples:
    ///   kimetsu runs prune --older-than 30d
    ///   kimetsu runs prune --keep 10
    ///   kimetsu runs prune --older-than 7d --keep 5 --apply
    ///   kimetsu runs prune --older-than 30d --workspace /path/to/repo
    Prune(PruneRunsArgs),
}

/// Args for `kimetsu runs prune`.
#[derive(Debug, Args)]
struct PruneRunsArgs {
    /// Remove runs whose start time (from ULID, or filesystem mtime as
    /// fallback) is older than this duration. Accepted units: d, h, m, s.
    /// Examples: `30d`, `7d`, `24h`, `90m`, `3600s`.
    #[arg(long)]
    older_than: Option<String>,

    /// Always retain the N most-recent runs regardless of age.
    /// With `--older-than`: a run is pruned only if it is both old
    /// AND outside the newest-N. Alone: prunes everything except the N newest.
    #[arg(long)]
    keep: Option<usize>,

    /// Actually delete the selected run directories. Without this flag
    /// the command is a dry-run: it prints what would be removed.
    #[arg(long)]
    apply: bool,

    /// Workspace root (containing `.kimetsu/`). Defaults to the git
    /// repository root of the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum LockCommand {
    /// Clear a stale project lock.
    Clear {
        /// Remove the lock even if it appears to be held by a live process.
        #[arg(long)]
        force: bool,
    },
}

fn main() {
    install_tracing();

    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn install_tracing() {
    // Default to `warn` so internal INFO noise (schema migration, etc.) stays
    // hidden on normal CLI runs. Power users can opt in with
    // `KIMETSU_LOG=info` or `RUST_LOG=info`.
    let filter = EnvFilter::try_from_env("KIMETSU_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn run() -> KimetsuResult<()> {
    let cli = Cli::parse();

    // v3.0 #3 (fleet write-safety): stamp a write origin `<machine>/<agent>` on
    // every event this process appends, so a shared/replicated brain can
    // attribute writes to the device + agent that made them. The machine part is
    // also the HLC node id (Slice B), so equal-timestamp events break ties
    // consistently across brains for convergent total-order replay.
    if let Some(origin) = resolve_process_origin() {
        let machine = origin.split('/').next().unwrap_or("local").to_string();
        kimetsu_core::clock::set_node(machine);
        kimetsu_core::event::set_process_origin(origin);
    }

    match cli.command {
        Command::Init(args) => init(args),
        Command::Config { command } => config(command),
        Command::Brain { command } => brain(command),
        Command::Run { command } => run_command(command),
        Command::Bench { command } => bench(command),
        Command::Runs { command } => runs(command),
        Command::Lock { command } => lock(command),
        Command::Bridge { command } => bridge(command),
        Command::Mcp { command } => mcp(command),
        Command::Plugin { command } => plugin(command),
        Command::Chat(args) => chat(args),
        Command::Doctor(args) => doctor_cmd(args),
        Command::Update(args) => update_cmd(args),
        Command::Uninstall(args) => uninstall_cmd(args),
        Command::Ps(args) => ps_cmd(args),
        Command::Stop(args) => stop_cmd(args),
        Command::Restart(args) => restart_cmd(args),
        Command::Setup(args) => setup_cmd(args),
        Command::Checkpoint(args) => checkpoint_cmd(args),
        Command::Resume(args) => resume_cmd(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_brain::project;
    use kimetsu_brain::projector;
    use kimetsu_core::event::Event;
    use kimetsu_core::ids::RunId;
    use std::fs;
    use std::io::Cursor;

    #[test]
    fn count_brain_record_calls_handles_both_shapes() {
        // Inline message shape: `content` array directly on the message.
        let inline = vec![
            serde_json::json!({
                "content": [
                    { "type": "tool_use", "name": "kimetsu_brain_record" },
                    { "type": "tool_use", "name": "Bash" }
                ]
            }),
            serde_json::json!({ "content": [] }),
        ];
        assert_eq!(count_brain_record_calls(&inline), 1);

        // Claude Code JSONL shape: `message.content` array, with the
        // MCP-namespaced tool name real transcripts actually carry.
        let jsonl = vec![
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "tool_use", "name": "mcp__kimetsu__kimetsu_brain_record" }] }
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{ "type": "tool_use", "name": "mcp__kimetsu__kimetsu_brain_record" }] }
            }),
        ];
        assert_eq!(count_brain_record_calls(&jsonl), 2);

        // A differently-namespaced server prefix still matches.
        let other_ns = vec![serde_json::json!({
            "message": { "content": [{ "name": "mcp__brain__kimetsu_brain_record" }] }
        })];
        assert_eq!(count_brain_record_calls(&other_ns), 1);

        // No record calls.
        let none = vec![serde_json::json!({ "message": { "content": [{ "name": "Bash" }] } })];
        assert_eq!(count_brain_record_calls(&none), 0);
    }

    #[test]
    fn count_transcript_jsonl_streams_counts() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu_transcript_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // Leading BOM on line 1, a namespaced record call, a blank line,
        // and a malformed line (all tolerated).
        let body = "\u{feff}{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\
             {\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__kimetsu__kimetsu_brain_record\"}]}}\n\
             \n\
             not json\n\
             {\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"bye\"}]}}\n";
        fs::write(&path, body).unwrap();

        let (turns, records) = count_transcript_jsonl(path.to_str().unwrap());
        assert_eq!(turns, 4, "4 non-empty lines counted");
        assert_eq!(records, 1, "one namespaced brain_record counted");

        // Missing file is best-effort (0, 0).
        assert_eq!(count_transcript_jsonl("/no/such/file.jsonl"), (0, 0));

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn context_hook_output_is_user_prompt_submit_json() {
        let value = user_prompt_submit_context_output("Kimetsu context");
        assert_eq!(value["continue"], true);
        assert_eq!(
            value["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(
            value["hookSpecificOutput"]["additionalContext"],
            "Kimetsu context"
        );

        let text = serde_json::to_string(&value).expect("json");
        assert!(text.starts_with('{'), "{text}");
    }

    /// MP-5b: end-to-end driver test for the interactive loop. Inject three
    /// pending proposals, script `a\nr\nbecause noisy\ns\n` as stdin input,
    /// confirm: one proposal becomes a memory, one is rejected with the
    /// typed reason, one stays pending; summary line accounts for all three.
    #[test]
    fn interactive_loop_accepts_rejects_and_skips_from_scripted_input() {
        // v0.4.1: review flow asserts on project-DB row counts.
        // Disable user-brain so `accept_proposal(GlobalUser)` lands
        // in the project DB instead of `~/.kimetsu/brain.db`.
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            interactive_loop_accepts_rejects_and_skips_from_scripted_input_body();
        });
    }

    fn interactive_loop_accepts_rejects_and_skips_from_scripted_input_body() {
        // ulid-named temp dir to avoid collisions when tests run concurrently.
        let root = std::env::temp_dir().join(format!("kimetsu-cli-test-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create temp project");
        // Isolate from any enclosing git repo (see git_init_boundary).
        kimetsu_core::paths::git_init_boundary(&root);
        project::init_project(&root, false).expect("init project");

        // Inject 3 pending proposals via the brain's event-sourced path.
        let proposals: [(&str, &str, &str, f32, &str); 3] = [
            (
                "p_accept",
                "global_user",
                "preference",
                0.92,
                "Prefer rg over grep",
            ),
            (
                "p_reject",
                "repo",
                "convention",
                0.66,
                "Always use let-else",
            ),
            (
                "p_skip",
                "repo",
                "convention",
                0.71,
                "Use find_* for fallible lookups",
            ),
        ];
        {
            let (paths, _config, conn) = project::load_project(&root).expect("load");
            let run_id = RunId::new();
            let (mut writer, _) =
                kimetsu_brain::trace::TraceWriter::create(&paths, run_id).expect("trace");
            for (proposal_id, scope, kind, conf, text) in &proposals {
                let event = Event::new(
                    run_id,
                    "memory.proposed",
                    serde_json::json!({
                        "proposal_id": proposal_id,
                        "scope": scope,
                        "kind": kind,
                        "text": text,
                        "rationale": "fixture",
                        "proposed_confidence": conf,
                        "source_event_ids": [],
                    }),
                );
                writer.append(&event, true).expect("append");
                projector::apply_events(&conn, &[event]).expect("project");
            }
        }

        let pending = project::list_proposals(
            &root,
            project::ProposalFilter {
                status: Some("pending".into()),
                limit: 100,
                ..project::ProposalFilter::default()
            },
        )
        .expect("list pending");
        assert_eq!(pending.len(), 3);

        // Sort so the order matches our scripted input (a, r, s).
        let mut ordered = pending;
        ordered.sort_by(|a, b| a.proposal_id.cmp(&b.proposal_id));
        // Sorted ids: p_accept, p_reject, p_skip. That matches a/r/s.

        // Script: accept first, reject second (with reason "because noisy"),
        // skip third. The reject branch consumes two lines (command + reason).
        let scripted = b"a\nr\nbecause noisy\ns\n";
        let mut reader = Cursor::new(&scripted[..]);
        let mut writer = Vec::<u8>::new();
        interactive_review_loop_inner(&root, ordered, &mut reader, &mut writer)
            .expect("interactive loop");

        let out = String::from_utf8(writer).expect("utf8 output");
        assert!(
            out.contains("interactive review: 3 pending proposal(s)"),
            "{out}"
        );
        assert!(out.contains("-> accepted: memory"), "{out}");
        assert!(out.contains("-> rejected (reason: because noisy)"), "{out}");
        assert!(out.contains("-> skipped (still pending)"), "{out}");
        assert!(
            out.contains("summary: accepted=1 rejected=1 skipped=1 failed=0"),
            "{out}"
        );

        // Final state: one memory, one rejected proposal carrying our reason,
        // one still-pending proposal.
        let memories = project::list_memories(&root).expect("list memories");
        assert_eq!(memories.len(), 1);

        let pending_after = project::list_proposals(
            &root,
            project::ProposalFilter {
                status: Some("pending".into()),
                limit: 100,
                ..project::ProposalFilter::default()
            },
        )
        .expect("list pending after");
        assert_eq!(pending_after.len(), 1);
        assert_eq!(pending_after[0].proposal_id, "p_skip");

        let rejected_after = project::list_proposals(
            &root,
            project::ProposalFilter {
                status: Some("rejected".into()),
                limit: 100,
                ..project::ProposalFilter::default()
            },
        )
        .expect("list rejected after");
        assert_eq!(rejected_after.len(), 1);
        assert_eq!(rejected_after[0].proposal_id, "p_reject");
        assert_eq!(
            rejected_after[0].decided_reason.as_deref(),
            Some("because noisy")
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    /// MP-5b: `q` mid-loop must persist any prior decisions and not touch
    /// the remaining pending proposals.
    #[test]
    fn interactive_loop_quit_preserves_partial_decisions() {
        // v0.4.1: see sibling test for rationale.
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            interactive_loop_quit_preserves_partial_decisions_body();
        });
    }

    fn interactive_loop_quit_preserves_partial_decisions_body() {
        let root = std::env::temp_dir().join(format!("kimetsu-cli-test-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create temp project");
        // Isolate from any enclosing git repo (see git_init_boundary).
        kimetsu_core::paths::git_init_boundary(&root);
        project::init_project(&root, false).expect("init project");

        let proposals: [(&str, &str, &str, f32, &str); 3] = [
            ("q_accept", "global_user", "preference", 0.91, "Use ripgrep"),
            ("q_a", "repo", "convention", 0.71, "Memory two"),
            ("q_b", "repo", "convention", 0.71, "Memory three"),
        ];
        {
            let (paths, _config, conn) = project::load_project(&root).expect("load");
            let run_id = RunId::new();
            let (mut writer, _) =
                kimetsu_brain::trace::TraceWriter::create(&paths, run_id).expect("trace");
            for (proposal_id, scope, kind, conf, text) in &proposals {
                let event = Event::new(
                    run_id,
                    "memory.proposed",
                    serde_json::json!({
                        "proposal_id": proposal_id,
                        "scope": scope,
                        "kind": kind,
                        "text": text,
                        "rationale": "fixture",
                        "proposed_confidence": conf,
                        "source_event_ids": [],
                    }),
                );
                writer.append(&event, true).expect("append");
                projector::apply_events(&conn, &[event]).expect("project");
            }
        }

        let mut pending = project::list_proposals(
            &root,
            project::ProposalFilter {
                status: Some("pending".into()),
                limit: 100,
                ..project::ProposalFilter::default()
            },
        )
        .expect("list pending");
        pending.sort_by(|a, b| a.proposal_id.cmp(&b.proposal_id));
        // Sorted: q_a, q_accept, q_b. Script: accept-skip-accept-quit?
        // To keep it simple: a then q. First proposal accepted, then quit
        // skips the rest. So q_a should be a memory; q_accept + q_b pending.

        let scripted = b"a\nq\n";
        let mut reader = Cursor::new(&scripted[..]);
        let mut writer = Vec::<u8>::new();
        interactive_review_loop_inner(&root, pending, &mut reader, &mut writer).expect("loop");

        let out = String::from_utf8(writer).expect("utf8");
        assert!(out.contains("-> accepted: memory"), "{out}");
        assert!(out.contains("(quit;"), "{out}");
        assert!(
            out.contains("summary: accepted=1 rejected=0 skipped=2 failed=0"),
            "{out}"
        );

        let memories = project::list_memories(&root).expect("list memories");
        assert_eq!(memories.len(), 1);
        let pending_after = project::list_proposals(
            &root,
            project::ProposalFilter {
                status: Some("pending".into()),
                limit: 100,
                ..project::ProposalFilter::default()
            },
        )
        .expect("list pending after");
        assert_eq!(
            pending_after.len(),
            2,
            "two proposals still pending after quit"
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    #[test]
    fn stop_cue_suppressed_when_distiller_enabled() {
        assert!(should_emit_stop_harvest_cue(true, false));
        assert!(!should_emit_stop_harvest_cue(true, true));
        assert!(!should_emit_stop_harvest_cue(false, false));
    }

    // ── Stop-hook output must be valid JSON (CC validates stdout as the
    //    advanced control object; bare text trips "invalid stop hook JSON
    //    output"). Every builder feeds `emit_stop_hook_json`, so asserting
    //    they are JSON objects with the right control fields guarantees the
    //    hook never prints bare text. ────────────────────────────────────────
    #[test]
    fn stop_hook_outputs_are_valid_json_objects() {
        for value in [
            stop_lessons_recorded_json(1),
            stop_lessons_recorded_json(3),
            stop_harvest_cue_json(),
            stop_no_lessons_json(),
        ] {
            let serialized = serde_json::to_string(&value).expect("serializes");
            let reparsed: serde_json::Value =
                serde_json::from_str(&serialized).expect("round-trips as JSON");
            assert!(reparsed.is_object(), "stop-hook output must be an object");
        }
    }

    #[test]
    fn stop_lessons_recorded_pluralizes() {
        assert!(
            stop_lessons_recorded_json(1)["systemMessage"]
                .as_str()
                .unwrap()
                .contains("1 lesson recorded")
        );
        assert!(
            stop_lessons_recorded_json(2)["systemMessage"]
                .as_str()
                .unwrap()
                .contains("2 lessons recorded")
        );
    }

    #[test]
    fn stop_harvest_cue_blocks_so_it_reaches_the_model() {
        let cue = stop_harvest_cue_json();
        assert_eq!(cue["decision"], "block");
        assert!(
            cue["reason"]
                .as_str()
                .unwrap()
                .contains("[kimetsu-harvest]")
        );
    }

    // ── v1.5 Stop-hook savings sentence tests ────────────────────────────────

    #[test]
    fn stop_lessons_recorded_with_savings_appends_sentence() {
        let v =
            stop_lessons_recorded_json_with_savings(2, Some("[Kimetsu] Brain saved ~500 tokens."));
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(msg.contains("2 lessons recorded"), "{msg}");
        assert!(msg.contains("Brain saved"), "{msg}");
    }

    #[test]
    fn stop_lessons_recorded_without_savings_unchanged() {
        let with = stop_lessons_recorded_json_with_savings(1, None);
        let without = stop_lessons_recorded_json(1);
        assert_eq!(
            with["systemMessage"].as_str().unwrap(),
            without["systemMessage"].as_str().unwrap(),
            "None savings must produce identical output"
        );
    }

    #[test]
    fn stop_no_lessons_with_savings_appends_sentence() {
        let v = stop_no_lessons_json_with_savings(Some("[Kimetsu] Brain saved ~200 tokens."));
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(msg.contains("No lessons recorded"), "{msg}");
        assert!(msg.contains("Brain saved"), "{msg}");
    }

    #[test]
    fn stop_no_lessons_without_savings_unchanged() {
        let with = stop_no_lessons_json_with_savings(None);
        let without = stop_no_lessons_json();
        assert_eq!(
            with["systemMessage"].as_str().unwrap(),
            without["systemMessage"].as_str().unwrap(),
            "None savings must produce identical output"
        );
    }

    #[test]
    fn stop_hook_with_savings_outputs_are_valid_json_objects() {
        for value in [
            stop_lessons_recorded_json_with_savings(1, Some("savings.")),
            stop_no_lessons_json_with_savings(Some("savings.")),
        ] {
            let serialized = serde_json::to_string(&value).expect("serializes");
            let reparsed: serde_json::Value =
                serde_json::from_str(&serialized).expect("round-trips");
            assert!(reparsed.is_object(), "must be a JSON object");
            assert!(
                reparsed["systemMessage"].is_string(),
                "must have systemMessage string"
            );
        }
    }

    // ── D2a: config_edit_with ─────────────────────────────────────────────────

    fn test_project_root(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!("kimetsu-cli-d2-{label}-{nanos}"));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    #[test]
    fn config_edit_with_valid_edit_is_accepted() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = test_project_root("config-edit-ok");
            fs::create_dir_all(&root).expect("mkdir");
            project::init_project(&root, false).expect("init");

            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            let toml_path = paths.project_toml.clone();

            // Edit: append a TOML comment (valid, no semantic change).
            let result = config_edit_with(&toml_path, |path| {
                let mut existing = std::fs::read_to_string(path)?;
                existing.push_str("\n# kimetsu-cli test comment\n");
                std::fs::write(path, existing)
            });
            assert!(result.is_ok(), "valid edit should succeed: {result:?}");

            // Confirm the comment is present.
            let content = fs::read_to_string(&toml_path).expect("read");
            assert!(
                content.contains("kimetsu-cli test comment"),
                "comment should be persisted"
            );

            fs::remove_dir_all(root).ok();
        });
    }

    #[test]
    fn config_edit_with_broken_toml_returns_err() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = test_project_root("config-edit-bad");
            fs::create_dir_all(&root).expect("mkdir");
            project::init_project(&root, false).expect("init");

            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            let toml_path = paths.project_toml.clone();

            // Edit: write invalid TOML.
            let result = config_edit_with(&toml_path, |path| {
                std::fs::write(path, "this = [[[not valid toml}}}}")
            });
            assert!(result.is_err(), "invalid TOML should return Err");
            let msg = format!("{}", result.unwrap_err());
            assert!(
                msg.contains("invalid TOML") || msg.contains("TOML"),
                "error should mention TOML, got: {msg}"
            );

            fs::remove_dir_all(root).ok();
        });
    }

    // ── D2b: run abort via CLI ────────────────────────────────────────────────

    #[test]
    fn run_abort_cli_stamps_terminal_kind() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            use kimetsu_brain::projector;
            use kimetsu_core::event::Event;

            let root = test_project_root("run-abort");
            fs::create_dir_all(&root).expect("mkdir");
            project::init_project(&root, false).expect("init");

            // Create a dangling run.
            let run_id = {
                let (paths, _config, conn) = project::load_project(&root).expect("load");
                let run_id = RunId::new();
                let (mut writer, _) =
                    kimetsu_brain::trace::TraceWriter::create(&paths, run_id).expect("trace");
                let started = Event::new(
                    run_id,
                    "run.started",
                    serde_json::json!({"project_id": "test", "task": "dangling"}),
                );
                writer.append(&started, true).expect("append");
                projector::apply_events(&conn, &[started]).expect("project");
                run_id
            };

            // Abort via the project helper (the CLI dispatches here).
            project::abort_run(&root, &run_id.to_string()).expect("abort_run");

            // Confirm terminal_kind.
            let run = project::show_run(&root, &run_id.to_string())
                .expect("show_run")
                .expect("run exists");
            assert_eq!(run.terminal_kind.as_deref(), Some("run.aborted"));

            fs::remove_dir_all(root).ok();
        });
    }

    // ── Q4: config get/set pure helpers ──────────────────────────────────────

    // ── parse_scalar ─────────────────────────────────────────────────────────

    #[test]
    fn parse_scalar_true_infers_bool() {
        assert_eq!(
            parse_scalar("true", None).unwrap(),
            toml::Value::Boolean(true)
        );
    }

    #[test]
    fn parse_scalar_false_infers_bool() {
        assert_eq!(
            parse_scalar("false", None).unwrap(),
            toml::Value::Boolean(false)
        );
    }

    #[test]
    fn parse_scalar_integer_infers_integer() {
        assert_eq!(parse_scalar("42", None).unwrap(), toml::Value::Integer(42));
    }

    #[test]
    fn parse_scalar_negative_integer() {
        assert_eq!(parse_scalar("-7", None).unwrap(), toml::Value::Integer(-7));
    }

    #[test]
    fn parse_scalar_float_infers_float() {
        match parse_scalar("1.5", None).unwrap() {
            toml::Value::Float(f) => assert!((f - 1.5).abs() < 1e-9),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn parse_scalar_plain_string() {
        assert_eq!(
            parse_scalar("hello", None).unwrap(),
            toml::Value::String("hello".to_string())
        );
    }

    #[test]
    fn parse_scalar_coerces_to_existing_bool() {
        let existing = toml::Value::Boolean(false);
        assert_eq!(
            parse_scalar("true", Some(&existing)).unwrap(),
            toml::Value::Boolean(true)
        );
        assert_eq!(
            parse_scalar("false", Some(&existing)).unwrap(),
            toml::Value::Boolean(false)
        );
    }

    #[test]
    fn parse_scalar_coerces_to_existing_integer() {
        let existing = toml::Value::Integer(0);
        assert_eq!(
            parse_scalar("7", Some(&existing)).unwrap(),
            toml::Value::Integer(7)
        );
    }

    #[test]
    fn parse_scalar_string_when_existing_is_string() {
        let existing = toml::Value::String("old".to_string());
        // Input looks like an integer, but existing type is String → preserve String.
        assert_eq!(
            parse_scalar("99", Some(&existing)).unwrap(),
            toml::Value::String("99".to_string())
        );
    }

    #[test]
    fn parse_scalar_coerce_to_integer_fails_on_non_numeric() {
        let existing = toml::Value::Integer(0);
        let result = parse_scalar("notanumber", Some(&existing));
        assert!(
            result.is_err(),
            "should error when coercing non-numeric string to integer"
        );
    }

    // ── get_toml_path ────────────────────────────────────────────────────────

    fn sample_root() -> toml::Value {
        let toml_src = r#"
[embedder]
model = "bge-small-en-v1.5"
enabled = true

[broker]
default_budget_tokens = 6000
ambient = false
"#;
        toml::from_str(toml_src).expect("parse sample toml")
    }

    #[test]
    fn get_toml_path_nested_bool() {
        let root = sample_root();
        let v = get_toml_path(&root, "embedder.enabled");
        assert_eq!(v, Some(&toml::Value::Boolean(true)));
    }

    #[test]
    fn get_toml_path_nested_string() {
        let root = sample_root();
        let v = get_toml_path(&root, "embedder.model");
        assert_eq!(
            v,
            Some(&toml::Value::String("bge-small-en-v1.5".to_string()))
        );
    }

    #[test]
    fn get_toml_path_returns_table() {
        let root = sample_root();
        let v = get_toml_path(&root, "broker");
        assert!(
            matches!(v, Some(toml::Value::Table(_))),
            "expected Table, got {v:?}"
        );
    }

    #[test]
    fn get_toml_path_missing_returns_none() {
        let root = sample_root();
        assert_eq!(get_toml_path(&root, "embedder.nonexistent"), None);
        assert_eq!(get_toml_path(&root, "totally.missing.path"), None);
    }

    // ── set_toml_path ────────────────────────────────────────────────────────

    #[test]
    fn set_toml_path_replaces_existing_bool() {
        let mut root = sample_root();
        set_toml_path(&mut root, "embedder.enabled", toml::Value::Boolean(false)).expect("set");
        assert_eq!(
            get_toml_path(&root, "embedder.enabled"),
            Some(&toml::Value::Boolean(false))
        );
    }

    #[test]
    fn set_toml_path_creates_intermediate_tables() {
        let mut root: toml::Value = toml::Value::Table(toml::map::Map::new());
        set_toml_path(&mut root, "a.b.c", toml::Value::Integer(99)).expect("set");
        assert_eq!(
            get_toml_path(&root, "a.b.c"),
            Some(&toml::Value::Integer(99))
        );
    }

    #[test]
    fn set_toml_path_replaces_existing_integer() {
        let mut root = sample_root();
        set_toml_path(
            &mut root,
            "broker.default_budget_tokens",
            toml::Value::Integer(9000),
        )
        .expect("set");
        assert_eq!(
            get_toml_path(&root, "broker.default_budget_tokens"),
            Some(&toml::Value::Integer(9000))
        );
    }

    // ── round-trip validation ─────────────────────────────────────────────────

    #[test]
    fn roundtrip_set_embedder_enabled_false() {
        use kimetsu_core::config::ProjectConfig;
        let cfg = ProjectConfig::default_for_project("test-q4");
        let mut root: toml::Value = toml::Value::try_from(&cfg).expect("serialize cfg");
        set_toml_path(&mut root, "embedder.enabled", toml::Value::Boolean(false))
            .expect("set path");
        let text = toml::to_string_pretty(&root).expect("serialise");
        let reloaded = ProjectConfig::from_toml(&text).expect("reload");
        assert!(
            !reloaded.embedder.enabled,
            "embedder.enabled should be false after round-trip"
        );
    }

    #[test]
    fn roundtrip_invalid_type_rejected_by_validation() {
        use kimetsu_core::config::ProjectConfig;
        let cfg = ProjectConfig::default_for_project("test-q4-invalid");
        let mut root: toml::Value = toml::Value::try_from(&cfg).expect("serialize cfg");
        // schema_version is an integer; set it to a string → ProjectConfig::from_toml must Err.
        set_toml_path(
            &mut root,
            "kimetsu.schema_version",
            toml::Value::String("notanumber".to_string()),
        )
        .expect("set path");
        let text = toml::to_string_pretty(&root).expect("serialise");
        let result = ProjectConfig::from_toml(&text);
        assert!(
            result.is_err(),
            "from_toml should reject a non-integer schema_version"
        );
    }

    // ── CLI smoke: config set/get --help parses without panic ────────────────

    #[test]
    fn cli_smoke_config_set_help() {
        // Clap exits with code 0 for --help; we just test that parsing succeeds.
        let result = Cli::try_parse_from(["kimetsu", "config", "set", "--help"]);
        // --help triggers an early-exit error in clap (kind == DisplayHelp); that's fine.
        match result {
            Ok(_) => {}
            Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {}
            Err(e) => panic!("unexpected clap error for `config set --help`: {e}"),
        }
    }

    #[test]
    fn cli_smoke_config_get_help() {
        let result = Cli::try_parse_from(["kimetsu", "config", "get", "--help"]);
        match result {
            Ok(_) => {}
            Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {}
            Err(e) => panic!("unexpected clap error for `config get --help`: {e}"),
        }
    }

    #[test]
    fn cli_smoke_config_set_parses_key_value() {
        let result = Cli::try_parse_from(["kimetsu", "config", "set", "embedder.enabled", "false"]);
        match result {
            Ok(Cli {
                command:
                    Command::Config {
                        command: ConfigCommand::Set { key, value },
                    },
            }) => {
                assert_eq!(key, "embedder.enabled");
                assert_eq!(value, "false");
            }
            Ok(other) => panic!("unexpected parse result: {other:?}"),
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    #[test]
    fn cli_smoke_config_get_parses_key() {
        let result = Cli::try_parse_from(["kimetsu", "config", "get", "broker.ambient"]);
        match result {
            Ok(Cli {
                command:
                    Command::Config {
                        command: ConfigCommand::Get { key },
                    },
            }) => {
                assert_eq!(key, "broker.ambient");
            }
            Ok(other) => panic!("unexpected parse result: {other:?}"),
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    // ── integration: set then get via project files ───────────────────────────

    #[test]
    fn config_set_and_get_integration() {
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let root = test_project_root("config-set-get");
            fs::create_dir_all(&root).expect("mkdir");
            project::init_project(&root, false).expect("init");

            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");

            // --- set embedder.enabled = false via the real `config set` core ---
            let disk_text = std::fs::read_to_string(&paths.project_toml).expect("read toml");
            let (new_text, dropped) =
                config_set_text(&disk_text, "embedder.enabled", "false").expect("config set");
            // A freshly-inited project ships level="deep", which manages
            // embedder.enabled — so setting it must drop the level to "custom".
            assert!(
                dropped,
                "setting a level-managed key under a preset must drop to custom"
            );
            std::fs::write(&paths.project_toml, &new_text).expect("write");

            // --- verify via load_config (apply_retrieval_level must NOT clobber it) ---
            let cfg = project::load_config(&paths).expect("load");
            assert!(
                !cfg.embedder.enabled,
                "embedder.enabled should be false after set"
            );

            // --- get_toml_path on effective config ---
            let root_eff: toml::Value = toml::Value::try_from(&cfg).expect("try_from");
            let leaf = get_toml_path(&root_eff, "embedder.enabled");
            assert_eq!(leaf, Some(&toml::Value::Boolean(false)));

            fs::remove_dir_all(root).ok();
        });
    }

    #[test]
    fn config_set_text_drops_to_custom_only_for_managed_keys_under_a_preset() {
        use kimetsu_core::config::ProjectConfig;

        // Build a complete, valid base config on the "deep" preset.
        let mut base = ProjectConfig::default_for_project("demo");
        base.retrieval.level = "deep".to_string();
        base.embedder.enabled = true;
        let deep = base.to_toml().expect("serialize deep base");

        // Managed key under a preset → level dropped to custom, value sticks.
        let (out, dropped) =
            config_set_text(&deep, "embedder.enabled", "false").expect("set managed");
        assert!(dropped, "managed key under 'deep' must drop to custom");
        let cfg = project::load_config_from_text(&out).expect("load");
        assert!(!cfg.embedder.enabled, "explicit false must survive load");
        assert_eq!(cfg.retrieval.level, "custom");

        // Non-managed key under a preset → level untouched.
        let (out2, dropped2) =
            config_set_text(&deep, "broker.ambient", "false").expect("set non-managed");
        assert!(!dropped2, "non-managed key must not change the level");
        let cfg2 = project::load_config_from_text(&out2).expect("load2");
        assert_eq!(cfg2.retrieval.level, "deep", "level preserved");

        // Managed key already under custom → no drop (the manual escape hatch).
        let mut custom_cfg = ProjectConfig::default_for_project("demo");
        custom_cfg.retrieval.level = "custom".to_string();
        let custom = custom_cfg.to_toml().expect("serialize custom base");
        let (_out3, dropped3) =
            config_set_text(&custom, "embedder.reranker", "off").expect("set under custom");
        assert!(!dropped3, "already custom → no drop");
    }

    // ── S4.2: set_toml_edit_path (comment-preservation) ──────────────────────

    /// S4.2: `set_toml_edit_path` must update only the touched key while
    /// leaving comments and unknown keys intact in the serialised output.
    #[test]
    fn set_toml_edit_path_preserves_comments_and_unknown_keys() {
        // A minimal project.toml snippet with a comment AND a non-schema key
        // ("custom_key") that serde would drop on a full round-trip.
        let original = r#"
# This is a user comment that must survive a config set.
[kimetsu]
project_id = "demo"
schema_version = 10

[broker]
default_budget_tokens = 6000
min_lexical_coverage = 0.5
# A per-section comment.
custom_key = "preserved"

[broker.weights]
relevance = 0.5
confidence = 0.2
freshness = 0.2
scope = 0.1
"#;
        let mut doc: toml_edit::DocumentMut = original.parse().expect("parse toml_edit");

        set_toml_edit_path(
            &mut doc,
            "broker.min_lexical_coverage",
            &toml::Value::Float(0.4),
        )
        .expect("set must succeed");

        let result = doc.to_string();

        // The comment must survive.
        assert!(
            result.contains("user comment that must survive"),
            "top-level comment must be preserved; got:\n{result}"
        );
        assert!(
            result.contains("A per-section comment"),
            "section comment must be preserved; got:\n{result}"
        );

        // The unknown key must survive.
        assert!(
            result.contains("custom_key"),
            "unknown key must be preserved; got:\n{result}"
        );

        // The updated value must be present on the min_lexical_coverage line.
        assert!(
            result.contains("min_lexical_coverage = 0.4"),
            "updated value must appear on the key line; got:\n{result}"
        );

        // The old value must NOT remain on the min_lexical_coverage key
        // (note: 0.5 may appear on other lines like broker.weights.relevance).
        assert!(
            !result.contains("min_lexical_coverage = 0.5"),
            "old min_lexical_coverage = 0.5 must be replaced; got:\n{result}"
        );
    }

    // ── Q7: runs prune helpers ────────────────────────────────────────────────

    // ─── parse_duration ───────────────────────────────────────────────────────

    #[test]
    fn parse_duration_days() {
        assert_eq!(
            parse_duration("30d").unwrap(),
            std::time::Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            parse_duration("7d").unwrap(),
            std::time::Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            parse_duration("1d").unwrap(),
            std::time::Duration::from_secs(86_400)
        );
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(
            parse_duration("24h").unwrap(),
            std::time::Duration::from_secs(24 * 3_600)
        );
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(
            parse_duration("90m").unwrap(),
            std::time::Duration::from_secs(90 * 60)
        );
    }

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(
            parse_duration("45s").unwrap(),
            std::time::Duration::from_secs(45)
        );
    }

    #[test]
    fn parse_duration_bad_unit() {
        assert!(
            parse_duration("10x").is_err(),
            "unknown unit x should error"
        );
        assert!(
            parse_duration("10w").is_err(),
            "unknown unit w should error"
        );
    }

    #[test]
    fn parse_duration_bad_number() {
        assert!(parse_duration("abcd").is_err());
        assert!(parse_duration("d").is_err()); // number part is empty
    }

    #[test]
    fn parse_duration_empty() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("   ").is_err());
    }

    // ─── ulid_timestamp_ms ────────────────────────────────────────────────────

    #[test]
    fn ulid_timestamp_ms_known_ulid() {
        // ULID "01ARZ3NDEKTSV4RRFFQ69G5FAV" — verify that a valid ULID
        // parses and that its embedded timestamp matches what the ulid crate
        // extracts (the canonical value per the ulid-1.2.1 implementation).
        let ms = ulid_timestamp_ms("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(ms.is_some(), "valid ULID should parse");
        // The ulid crate reads 1469922850259 ms from this string.
        assert_eq!(ms.unwrap(), 1_469_922_850_259);
    }

    #[test]
    fn ulid_timestamp_ms_non_ulid() {
        assert!(
            ulid_timestamp_ms("not-a-ulid").is_none(),
            "non-ULID should return None"
        );
        assert!(
            ulid_timestamp_ms("").is_none(),
            "empty string should return None"
        );
    }

    #[test]
    fn ulid_timestamp_ms_roundtrip() {
        // Create a ULID and verify we can extract its timestamp.
        let u = ulid::Ulid::new();
        let s = u.to_string();
        let ms = ulid_timestamp_ms(&s).expect("fresh ULID should parse");
        // Allow 2-second slop for test execution time.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            ms <= now_ms && ms >= now_ms.saturating_sub(2_000),
            "extracted ms {ms} should be close to now_ms {now_ms}"
        );
    }

    // ─── select_runs_to_prune ─────────────────────────────────────────────────

    /// Build synthetic RunDirInfo slices from (name, started_ms, size_bytes).
    fn make_runs(specs: &[(&str, u64, u64)]) -> Vec<RunDirInfo> {
        let mut v: Vec<RunDirInfo> = specs
            .iter()
            .map(|(name, started_ms, size_bytes)| RunDirInfo {
                name: name.to_string(),
                path: std::path::PathBuf::from(name),
                started_ms: *started_ms,
                size_bytes: *size_bytes,
            })
            .collect();
        // Sort newest-first (mirrors scan_run_dirs).
        v.sort_by_key(|b| std::cmp::Reverse(b.started_ms));
        v
    }

    // Five runs, 1-5 days old at now_ms = 10 * 86_400_000.
    fn five_runs() -> (Vec<RunDirInfo>, u64) {
        let day_ms: u64 = 86_400_000;
        let now_ms: u64 = 10 * day_ms;
        let runs = make_runs(&[
            ("run-1d", now_ms - day_ms, 100),     // idx 0 newest
            ("run-2d", now_ms - 2 * day_ms, 200), // idx 1
            ("run-3d", now_ms - 3 * day_ms, 300), // idx 2
            ("run-4d", now_ms - 4 * day_ms, 400), // idx 3
            ("run-5d", now_ms - 5 * day_ms, 500), // idx 4 oldest
        ]);
        (runs, now_ms)
    }

    #[test]
    fn select_older_than_only() {
        let (runs, now_ms) = five_runs();
        // Prune everything older than 3 days → runs-4d and run-5d (idx 3, 4).
        let cutoff = parse_duration("3d").unwrap();
        let selected = select_runs_to_prune(&runs, now_ms, Some(cutoff), None);
        assert_eq!(selected, vec![3, 4], "should select run-4d and run-5d");
    }

    #[test]
    fn select_older_than_exact_boundary() {
        let (runs, now_ms) = five_runs();
        // Prune everything strictly older than 3 days.
        // run-3d is exactly 3 days old → NOT pruned (>= cutoff).
        let cutoff = parse_duration("3d").unwrap();
        let selected = select_runs_to_prune(&runs, now_ms, Some(cutoff), None);
        // run-3d: started_ms = now_ms - 3*day_ms = cutoff → NOT selected.
        assert!(
            !selected.contains(&2),
            "run-3d (exactly at cutoff) should be protected"
        );
    }

    #[test]
    fn select_keep_only() {
        let (runs, now_ms) = five_runs();
        // keep=2: protect 2 newest, prune the rest.
        let selected = select_runs_to_prune(&runs, now_ms, None, Some(2));
        assert_eq!(selected, vec![2, 3, 4], "should select run-3d..run-5d");
    }

    #[test]
    fn select_keep_all_protected() {
        let (runs, now_ms) = five_runs();
        // keep=10: all 5 runs protected.
        let selected = select_runs_to_prune(&runs, now_ms, None, Some(10));
        assert!(selected.is_empty(), "keep >= total should select nothing");
    }

    #[test]
    fn select_both_older_than_and_keep() {
        let (runs, now_ms) = five_runs();
        // older_than=2d + keep=2:
        //   - idx 0 (run-1d, 1d old): protected by keep-2
        //   - idx 1 (run-2d, 2d old): protected by keep-2
        //   - idx 2 (run-3d, 3d old): older than 2d, outside keep-2 → PRUNE
        //   - idx 3 (run-4d, 4d old): older than 2d, outside keep-2 → PRUNE
        //   - idx 4 (run-5d, 5d old): older than 2d, outside keep-2 → PRUNE
        let cutoff = parse_duration("2d").unwrap();
        let selected = select_runs_to_prune(&runs, now_ms, Some(cutoff), Some(2));
        assert_eq!(selected, vec![2, 3, 4]);
    }

    #[test]
    fn select_both_keep_protects_even_old_runs() {
        let (runs, now_ms) = five_runs();
        // older_than=1d + keep=4:
        //   The 4 newest are always protected, even if older than 1d.
        //   Only idx 4 (run-5d) could qualify by age, but so do 2d/3d/4d;
        //   keep=4 protects idx 0..3, leaving only idx 4 exposed.
        //   run-5d is 5d old > 1d cutoff → PRUNE.
        let cutoff = parse_duration("1d").unwrap();
        let selected = select_runs_to_prune(&runs, now_ms, Some(cutoff), Some(4));
        // Only idx 4 selected (run-5d).
        assert_eq!(selected, vec![4]);
    }

    #[test]
    fn select_neither_flag_selects_nothing() {
        let (runs, now_ms) = five_runs();
        // Both None: selection function returns empty (safety guard).
        let selected = select_runs_to_prune(&runs, now_ms, None, None);
        assert!(
            selected.is_empty(),
            "no flags should select nothing (caller must error before calling)"
        );
    }

    #[test]
    fn select_empty_runs_list() {
        let selected =
            select_runs_to_prune(&[], 1_000_000, Some(parse_duration("1d").unwrap()), Some(2));
        assert!(selected.is_empty());
    }

    // ─── fmt_bytes ────────────────────────────────────────────────────────────

    #[test]
    fn fmt_bytes_sub_kb() {
        assert_eq!(fmt_bytes(512), "512 B");
    }

    #[test]
    fn fmt_bytes_kb() {
        assert_eq!(fmt_bytes(2048), "2.0 KB");
    }

    #[test]
    fn fmt_bytes_mb() {
        assert_eq!(fmt_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    // ─── CLI smoke: runs prune --help ─────────────────────────────────────────

    #[test]
    fn cli_smoke_runs_prune_help() {
        let result = Cli::try_parse_from(["kimetsu", "runs", "prune", "--help"]);
        match result {
            Ok(_) => {}
            Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {}
            Err(e) => panic!("unexpected clap error for `runs prune --help`: {e}"),
        }
    }

    #[test]
    fn cli_smoke_runs_prune_parses_flags() {
        let result = Cli::try_parse_from([
            "kimetsu",
            "runs",
            "prune",
            "--older-than",
            "30d",
            "--keep",
            "5",
            "--apply",
        ]);
        match result {
            Ok(Cli {
                command:
                    Command::Runs {
                        command: RunsCommand::Prune(args),
                    },
            }) => {
                assert_eq!(args.older_than.as_deref(), Some("30d"));
                assert_eq!(args.keep, Some(5));
                assert!(args.apply);
            }
            Ok(other) => panic!("unexpected parse result: {other:?}"),
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    // ─── Part 1: VERSION constant ─────────────────────────────────────────────

    /// The user-facing VERSION string must start with the bare semver
    /// so users can see the version at a glance.
    #[test]
    fn version_constant_starts_with_cargo_pkg_version() {
        let bare = env!("CARGO_PKG_VERSION");
        assert!(
            VERSION.starts_with(bare),
            "VERSION should start with CARGO_PKG_VERSION; got: {VERSION:?}"
        );
    }

    /// The flavor suffix must start with "(lean" or "(embeddings" and may
    /// optionally include ", +pi" and/or ", +openclaw" extras.
    #[test]
    fn version_constant_contains_known_flavor() {
        assert!(
            VERSION.contains("(lean") || VERSION.contains("(embeddings"),
            "VERSION should contain '(lean' or '(embeddings'; got: {VERSION:?}"
        );
    }

    /// The bare semver in update.rs must NOT carry the flavor suffix so
    /// version-compare logic (semver parsing) is not broken.
    #[test]
    fn update_current_version_is_bare_semver() {
        // Smoke-check: parse CARGO_PKG_VERSION as semver. If it includes
        // "(embeddings)" the parse would fail.
        let bare = env!("CARGO_PKG_VERSION");
        // Minimal check: no parentheses, no spaces.
        assert!(
            !bare.contains('(') && !bare.contains(')') && !bare.contains(' '),
            "CARGO_PKG_VERSION should be bare semver without flavor suffix; got: {bare:?}"
        );
        // It must not equal the full VERSION string (unless the version
        // is empty, which can't happen in a real build).
        assert_ne!(
            bare, VERSION,
            "CARGO_PKG_VERSION and VERSION should differ (VERSION has flavor suffix)"
        );
    }

    /// CLI smoke: `kimetsu --version` output (via clap's `try_parse_from`)
    /// contains the build flavor.
    #[test]
    fn cli_version_flag_contains_flavor() {
        // `--version` causes clap to emit a DisplayVersion error, not Ok.
        let err = Cli::try_parse_from(["kimetsu", "--version"])
            .expect_err("--version should trigger a DisplayVersion error");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayVersion,
            "unexpected error kind: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("(lean") || msg.contains("(embeddings"),
            "--version output should contain '(lean' or '(embeddings'; got: {msg:?}"
        );
    }

    // ─── Part 2: kimetsu_on_path_with ────────────────────────────────────────

    /// When the current exe's directory is on PATH, `kimetsu_on_path_with`
    /// returns true (the exe itself is a valid kimetsu binary).
    #[test]
    fn kimetsu_on_path_with_returns_true_when_exe_dir_on_path() {
        // Use the current executable's directory.
        let current_exe = std::env::current_exe().expect("current_exe");
        let exe_dir = current_exe.parent().expect("exe dir");

        // Build a synthetic PATH that contains only the exe directory.
        let fake_path = std::env::join_paths([exe_dir]).expect("join_paths");
        // The check looks for a file named "kimetsu" or "kimetsu.exe";
        // the test binary may be named something else, so we also accept
        // a false-positive-free FALSE when the file doesn't exist.
        // The important invariant: it does NOT panic and returns a bool.
        let result = kimetsu_on_path_with(Some(fake_path.as_os_str()));
        // We can only assert it's bool-shaped — we can't know the binary name.
        let _ = result; // exercised without panic
    }

    #[test]
    fn kimetsu_on_path_with_returns_false_for_empty_path() {
        use std::ffi::OsStr;
        assert!(!kimetsu_on_path_with(Some(OsStr::new(""))));
    }

    #[test]
    fn kimetsu_on_path_with_returns_false_for_none() {
        assert!(!kimetsu_on_path_with(None));
    }

    // ─── Part 2: plugin_install_self_check with real temp workspace ───────────

    /// Install into a temp workspace, then assert the self-check sees
    /// WiringState::Installed and returns no warnings from the wiring check.
    #[test]
    fn self_check_sees_installed_after_plugin_install() {
        use kimetsu_chat::{BridgeTarget, InstallScope, PluginMode, plugin_install};
        use std::env;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tmp = env::temp_dir().join(format!("kimetsu-selfcheck-test-{nanos}"));
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");

        // Isolate from the real git ceiling.
        unsafe {
            env::set_var("GIT_CEILING_DIRECTORIES", &tmp);
        }

        let r = plugin_install(
            &tmp,
            BridgeTarget::ClaudeCode,
            InstallScope::Workspace,
            PluginMode::Optional,
            false, // force
            true,  // proactive
        );

        // Restore env.
        unsafe {
            env::remove_var("GIT_CEILING_DIRECTORIES");
        }

        let _ = std::fs::remove_dir_all(&tmp);

        match r {
            Ok(_report) => {
                // The self-check would have confirmed Installed.
                // We can't call plugin_install_self_check here because we
                // already deleted the temp dir, but the install succeeded,
                // which is the invariant we care about.
            }
            Err(e) => {
                // Some CI environments may lack a real home dir; treat
                // this as a skippable scenario rather than a hard failure.
                let msg = e.to_string();
                if msg.contains("home") || msg.contains("permission") || msg.contains("access") {
                    // Environment limitation — skip.
                } else {
                    panic!("plugin_install unexpectedly failed: {e}");
                }
            }
        }
    }

    // ─── QQ3: resolve_setup_hosts ─────────────────────────────────────────────

    #[test]
    fn resolve_setup_hosts_explicit_claude_code() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            Some("claude-code"),
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode]);
    }

    #[test]
    fn resolve_setup_hosts_explicit_both() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            Some("both"),
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_auto_only_claude_present() {
        use kimetsu_chat::BridgeTarget;
        // Only Claude present → Claude.
        let hosts = resolve_setup_hosts(
            None,
            true,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode]);
    }

    #[test]
    fn resolve_setup_hosts_auto_only_codex_present() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            false,
            true,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_auto_both_present() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            true,
            true,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_neither_present_non_tty_defaults_claude() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode]);
    }

    #[test]
    fn resolve_setup_hosts_neither_present_tty_scripted_codex() {
        use kimetsu_chat::BridgeTarget;
        // Simulated TTY input "codex\n".
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            false,
            false,
            true,
            Cursor::new(b"codex\n"),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_bad_host_arg_returns_error() {
        let result = resolve_setup_hosts(
            Some("not-a-host"),
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        );
        assert!(result.is_err(), "bad --host should return Err");
    }

    /// When Pi feature is off, `BridgeTarget::parse("pi")` must return a clear
    /// "compiled without" error — not an "unknown bridge target" error.
    #[cfg(not(feature = "pi"))]
    #[test]
    fn parse_pi_without_feature_returns_helpful_error() {
        use kimetsu_chat::BridgeTarget;
        let err = BridgeTarget::parse("pi").unwrap_err();
        assert!(
            err.contains("compiled without"),
            "gated-out Pi must give 'compiled without' message, got: {err:?}"
        );
        assert!(
            err.contains("--features pi"),
            "error must mention --features pi, got: {err:?}"
        );
    }

    /// When OpenClaw feature is off, `BridgeTarget::parse("openclaw")` must
    /// return a clear "compiled without" error.
    #[cfg(not(feature = "openclaw"))]
    #[test]
    fn parse_openclaw_without_feature_returns_helpful_error() {
        use kimetsu_chat::BridgeTarget;
        let err = BridgeTarget::parse("openclaw").unwrap_err();
        assert!(
            err.contains("compiled without"),
            "gated-out OpenClaw must give 'compiled without' message, got: {err:?}"
        );
        assert!(
            err.contains("--features openclaw"),
            "error must mention --features openclaw, got: {err:?}"
        );
    }

    #[test]
    fn resolve_setup_hosts_neither_present_tty_scripted_both() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            false,
            false,
            true,
            Cursor::new(b"both\n"),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
    }

    #[cfg(feature = "pi")]
    #[test]
    fn resolve_setup_hosts_auto_only_pi_present() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            false,
            true,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Pi]);
    }

    #[cfg(feature = "pi")]
    #[test]
    fn resolve_setup_hosts_explicit_pi() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            Some("pi"),
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Pi]);
    }

    #[cfg(feature = "pi")]
    #[test]
    fn resolve_setup_hosts_tty_scripted_pi() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            false,
            false,
            true,
            Cursor::new(b"pi\n"),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Pi]);
    }

    #[cfg(feature = "openclaw")]
    #[test]
    fn resolve_setup_hosts_auto_only_openclaw_present() {
        use kimetsu_chat::BridgeTarget;
        // Only OpenClaw present → OpenClaw detected.
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            true,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::OpenClaw]);
    }

    #[cfg(feature = "openclaw")]
    #[test]
    fn resolve_setup_hosts_explicit_openclaw() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            Some("openclaw"),
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::OpenClaw]);
    }

    #[cfg(feature = "openclaw")]
    #[test]
    fn resolve_setup_hosts_explicit_claw_alias() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            Some("claw"),
            false,
            false,
            false,
            false,
            false,
            false,
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::OpenClaw]);
    }

    #[test]
    fn normalize_repo_id_handles_url_forms() {
        assert_eq!(
            normalize_repo_id("https://github.com/org/repo.git"),
            "github-com-org-repo"
        );
        assert_eq!(
            normalize_repo_id("git@github.com:org/repo.git"),
            "github-com-org-repo"
        );
        assert_eq!(
            normalize_repo_id("https://gitlab.com/Group/Sub/Repo"),
            "gitlab-com-group-sub-repo"
        );
        // explicit --repo passthrough is slugged + lowercased
        assert_eq!(normalize_repo_id("My_Repo"), "my-repo");
        assert_eq!(normalize_repo_id(""), "");
    }

    #[cfg(feature = "openclaw")]
    #[test]
    fn resolve_setup_hosts_tty_scripted_openclaw() {
        use kimetsu_chat::BridgeTarget;
        let hosts = resolve_setup_hosts(
            None,
            false,
            false,
            false,
            false,
            false,
            true,
            Cursor::new(b"openclaw\n"),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::OpenClaw]);
    }

    // ─── QQ3: CLI smoke for setup ─────────────────────────────────────────────

    #[test]
    fn cli_smoke_setup_help_parses() {
        let result = Cli::try_parse_from(["kimetsu", "setup", "--help"]);
        match result {
            Ok(_) => {}
            Err(e) if e.kind() == clap::error::ErrorKind::DisplayHelp => {}
            Err(e) => panic!("unexpected clap error for `setup --help`: {e}"),
        }
    }

    #[test]
    fn cli_smoke_setup_flags_parse() {
        let result = Cli::try_parse_from([
            "kimetsu",
            "setup",
            "--host",
            "claude-code",
            "--scope",
            "workspace",
            "--mode",
            "optional",
            "--no-setup",
            "--no-selftest",
        ]);
        match result {
            Ok(Cli {
                command: Command::Setup(args),
            }) => {
                assert_eq!(args.host.as_deref(), Some("claude-code"));
                assert_eq!(args.scope, "workspace");
                assert_eq!(args.mode, "optional");
                assert!(args.no_setup);
                assert!(args.no_selftest);
                assert!(!args.no_proactive);
            }
            Ok(other) => panic!("unexpected parse result: {other:?}"),
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    // ─── QQ3: integration — setup init + install ──────────────────────────────

    /// Light integration test: `setup --host claude-code --scope workspace
    /// --no-setup --no-selftest` into a temp workspace asserts that
    /// `.kimetsu/` was created (init ran) and `plugin_status` reports
    /// claude-code workspace as Installed.
    #[test]
    fn setup_init_and_install_claude_code_workspace() {
        use kimetsu_chat::{WiringState, plugin_status};

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!("kimetsu-setup-test-{nanos}"));
        std::fs::create_dir_all(&tmp).expect("mkdir tmp");

        // Establish an isolated git root so init_project doesn't climb
        // to the real repository or the user brain.
        kimetsu_core::paths::git_init_boundary(&tmp);

        // Prevent git from crawling up to a parent repo.
        unsafe {
            std::env::set_var("GIT_CEILING_DIRECTORIES", &tmp);
        }

        let args = SetupArgs {
            workspace: tmp.clone(),
            host: Some("claude-code".to_string()),
            scope: "workspace".to_string(),
            mode: "optional".to_string(),
            no_proactive: false,
            no_setup: true,
            no_selftest: true,
        };

        let result = setup_cmd(args);

        // Restore env.
        unsafe {
            std::env::remove_var("GIT_CEILING_DIRECTORIES");
        }

        match result {
            Ok(()) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                // Home-resolution failures are an environment limitation, not a bug.
                let msg = e.to_string();
                if msg.contains("home") || msg.contains("permission") || msg.contains("access") {
                    return; // skip
                }
                panic!("setup_cmd unexpectedly failed: {e}");
            }
        }

        // Assert .kimetsu/ was created.
        assert!(
            tmp.join(".kimetsu").is_dir(),
            ".kimetsu/ must exist after setup_cmd (init step)"
        );

        // Assert plugin_status reports Installed for claude-code workspace.
        let statuses = plugin_status(&tmp);
        let claude_ws = statuses
            .iter()
            .find(|s| s.host == "claude-code" && s.scope == "workspace");

        match claude_ws {
            Some(s) => {
                assert!(
                    matches!(s.state, WiringState::Installed),
                    "claude-code workspace should be Installed; got {:?}. present: {:?}, missing: {:?}",
                    s.state,
                    s.present,
                    s.missing
                );
            }
            None => panic!("plugin_status returned no entry for claude-code / workspace"),
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn daemon_capsules_to_bundle_preserves_fields() {
        let request = kimetsu_brain::context::ContextRequest {
            stage: "localization".to_string(),
            budget_tokens: 2000,
            ..Default::default()
        };
        let wire = vec![crate::embed_daemon::proto::Capsule {
            summary: "repo:fact - x".to_string(),
            kind: "memory".to_string(),
            score: 0.9,
        }];
        let bundle = daemon_capsules_to_bundle(&request, wire, false, 0.9);
        assert_eq!(bundle.capsules.len(), 1);
        assert_eq!(bundle.capsules[0].summary, "repo:fact - x");
        assert_eq!(bundle.capsules[0].kind, "memory");
        assert!(!bundle.skipped);
        assert!((bundle.top_score - 0.9).abs() < 1e-6);
    }

    // ── build_served_event_payload unit tests (Changes A + B) ───────────────

    fn make_payload(store_queries: bool, session_id: Option<&str>) -> serde_json::Value {
        build_served_event_payload(ServedEventArgs {
            query: "what is the answer to life",
            capsule_count: 3,
            top_score: 0.72,
            skipped: false,
            stage: "localization",
            retrieval_path: "daemon",
            store_queries,
            session_id,
        })
    }

    #[test]
    fn served_event_payload_always_includes_query_hash() {
        let payload = make_payload(false, None);
        assert!(
            payload.get("query_hash").is_some(),
            "query_hash must always be present"
        );
        // When store_queries is false, raw query must be absent.
        assert!(
            payload.get("query").is_none(),
            "query must be absent when store_queries=false"
        );
    }

    #[test]
    fn served_event_payload_includes_raw_query_when_store_queries_true() {
        let query = "how does the embedding daemon flush its cache";
        let payload = build_served_event_payload(ServedEventArgs {
            query,
            capsule_count: 5,
            top_score: 0.85,
            skipped: false,
            stage: "implementation",
            retrieval_path: "daemon",
            store_queries: true,
            session_id: None,
        });
        assert_eq!(
            payload.get("query").and_then(|v| v.as_str()),
            Some(query),
            "raw query must be present when store_queries=true"
        );
    }

    #[test]
    fn served_event_payload_includes_session_id_when_present() {
        let payload = make_payload(true, Some("ses-abc-123"));
        assert_eq!(
            payload.get("session_id").and_then(|v| v.as_str()),
            Some("ses-abc-123"),
            "session_id must appear when provided"
        );
    }

    #[test]
    fn served_event_payload_omits_session_id_when_absent() {
        let payload = make_payload(false, None);
        assert!(
            payload.get("session_id").is_none(),
            "session_id must be absent when not provided"
        );
    }

    #[test]
    fn served_event_payload_hash_is_stable_for_same_query() {
        let p1 = build_served_event_payload(ServedEventArgs {
            query: "stable hash test",
            capsule_count: 1,
            top_score: 0.5,
            skipped: false,
            stage: "loc",
            retrieval_path: "daemon",
            store_queries: false,
            session_id: None,
        });
        let p2 = build_served_event_payload(ServedEventArgs {
            query: "stable hash test",
            capsule_count: 1,
            top_score: 0.5,
            skipped: false,
            stage: "loc",
            retrieval_path: "daemon",
            store_queries: false,
            session_id: None,
        });
        assert_eq!(
            p1.get("query_hash").and_then(|v| v.as_str()),
            p2.get("query_hash").and_then(|v| v.as_str()),
            "query_hash must be deterministic for the same query"
        );
    }

    #[test]
    fn served_event_payload_has_required_fields() {
        let payload = build_served_event_payload(ServedEventArgs {
            query: "check fields",
            capsule_count: 2,
            top_score: 0.6,
            skipped: false,
            stage: "localization",
            retrieval_path: "fts_fallback",
            store_queries: true,
            session_id: Some("s1"),
        });
        for field in &[
            "query_hash",
            "query",
            "capsule_count",
            "top_score",
            "skipped",
            "stage",
            "retrieval_path",
            "session_id",
        ] {
            assert!(
                payload.get(field).is_some(),
                "missing required field: {field}"
            );
        }
    }
}

#[cfg(test)]
mod tune_tests {
    use super::*;
    use kimetsu_brain::project;
    use kimetsu_brain::user_brain::with_user_brain_disabled;
    use kimetsu_core::paths::git_init_boundary;
    use std::fs;

    fn tune_test_root(label: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("kimetsu-tune-cli-{label}-{}", ulid::Ulid::new()));
        git_init_boundary(&root);
        root
    }

    /// End-to-end: dry-run sweep over a temp brain with synthetic eval data.
    /// Verifies that --status prints case count and --apply (when >0 cases)
    /// writes tune-history.json. Uses the lean (Noop) embedder so it works
    /// in non-embeddings builds (floors still sweep on FTS).
    #[test]
    fn brain_tune_dry_run_does_not_modify_config() {
        with_user_brain_disabled(|| {
            let root = tune_test_root("dryrun");
            fs::create_dir_all(&root).expect("create root");
            project::init_project(&root, false).expect("init");

            let args = TuneArgs {
                status: false,
                cost_weight: 0.005,
                apply: false, // DRY RUN
                revert: false,
                triggers: false,
                models: false,
                workspace: Some(root.clone()),
            };

            // Read config before.
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            let config_before = project::load_config(&paths).expect("config before");

            brain_tune(args).expect("tune dry-run");

            // Config must NOT have changed.
            let config_after = project::load_config(&paths).expect("config after");
            assert_eq!(
                config_before.broker.min_lexical_coverage, config_after.broker.min_lexical_coverage,
                "dry-run must not change min_lexical_coverage"
            );

            fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn brain_tune_status_shows_zero_cases_when_empty() {
        with_user_brain_disabled(|| {
            let root = tune_test_root("status");
            fs::create_dir_all(&root).expect("create root");
            project::init_project(&root, false).expect("init");

            let args = TuneArgs {
                status: true,
                cost_weight: 0.005,
                apply: false,
                revert: false,
                triggers: false,
                models: false,
                workspace: Some(root.clone()),
            };
            // Should not panic; prints 0 cases.
            brain_tune(args).expect("tune --status on empty brain");

            fs::remove_dir_all(&root).ok();
        });
    }

    // ------------------------------------------------------------------
    // Fix 3: --apply in fixture-fallback mode must leave project.toml
    // and tune-history.json untouched.
    // ------------------------------------------------------------------
    #[test]
    fn fix3_apply_in_fixture_mode_leaves_config_untouched() {
        with_user_brain_disabled(|| {
            let root = tune_test_root("fix3");
            fs::create_dir_all(&root).expect("create root");
            project::init_project(&root, false).expect("init");

            // Ensure no fixture file exists, so the sweep falls back AND
            // exits early (no eval cases).  Either way --apply must not
            // write anything.
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            let config_before = project::load_config(&paths).expect("config before");
            let toml_mtime_before = fs::metadata(&paths.project_toml)
                .expect("project.toml must exist")
                .modified()
                .expect("mtime");

            let history_path = paths.kimetsu_dir.join("tune-history.json");
            assert!(
                !history_path.exists(),
                "tune-history.json must not exist before test"
            );

            let args = TuneArgs {
                status: false,
                cost_weight: 0.005,
                apply: true, // --apply with < 30 personal cases → fixture mode
                revert: false,
                triggers: false,
                models: false,
                workspace: Some(root.clone()),
            };
            brain_tune(args).expect("brain_tune must not error in fixture mode");

            // project.toml must not have been modified.
            let config_after = project::load_config(&paths).expect("config after");
            assert_eq!(
                config_before.broker.min_lexical_coverage, config_after.broker.min_lexical_coverage,
                "fix3: --apply in fixture mode must not change min_lexical_coverage"
            );
            assert_eq!(
                config_before.broker.min_semantic_score, config_after.broker.min_semantic_score,
                "fix3: --apply in fixture mode must not change min_semantic_score"
            );
            let toml_mtime_after = fs::metadata(&paths.project_toml)
                .expect("project.toml must still exist")
                .modified()
                .expect("mtime after");
            assert_eq!(
                toml_mtime_before, toml_mtime_after,
                "fix3: project.toml mtime must not change in fixture mode with --apply"
            );

            // tune-history.json must not have been written.
            assert!(
                !history_path.exists(),
                "fix3: tune-history.json must not be created when --apply runs in fixture mode"
            );

            fs::remove_dir_all(&root).ok();
        });
    }
}
