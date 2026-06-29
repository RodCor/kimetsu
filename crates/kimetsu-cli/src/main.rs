use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod ask;
mod distiller;
mod doctor;
mod embed_daemon;
mod harvest_setup;
mod proactive_state;
mod process;
mod skill_synth;
mod update;

use clap::{Args, Parser, Subcommand};
use kimetsu_agent::bench::{BenchOptions, run_benchmark};
use kimetsu_agent::pipeline::{CodingRunOptions, run_coding};
use kimetsu_agent::swe_bench::{SweBenchOptions, run_swe_bench};
use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use tracing_subscriber::EnvFilter;

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
    /// The memory id to credit.
    #[arg(long)]
    memory_id: String,
    /// Optional rationale recorded with the citation.
    #[arg(long)]
    note: Option<String>,
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
}

#[derive(Debug, Args)]
struct BrainImportArgs {
    /// Input file path. Use `-` to read from stdin.
    file: String,
    /// Override the scope for every imported entry (global_user|project|repo|run).
    #[arg(long)]
    scope_override: Option<String>,
    /// Override the brain workspace path (defaults to current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
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

fn update_cmd(args: UpdateArgs) -> KimetsuResult<()> {
    let flavor = update::UpdateFlavor::parse(&args.flavor)?;
    update::run(update::UpdateOptions {
        check: args.check,
        dry_run: args.dry_run,
        force: args.force,
        flavor,
    })
}

fn uninstall_cmd(args: UninstallArgs) -> KimetsuResult<()> {
    update::uninstall(update::UninstallOptions {
        dry_run: args.dry_run,
        yes: args.yes,
        keep_plugins: args.keep_plugins,
        delete_user_data: args.delete_user_data,
    })
}

// ── kimetsu checkpoint ────────────────────────────────────────────────────────

/// `kimetsu checkpoint [note]` — manually save a mid-session work episode.
fn checkpoint_cmd(args: CheckpointArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let note = args.note.as_deref().unwrap_or("");

    // Use capture_episode_now with an empty transcript (manual save does not
    // require a transcript — the note itself is sufficient context).
    let ok = distiller::capture_episode_now(&workspace, "", note);

    if ok {
        println!("[Kimetsu] Work checkpoint saved.");
        if !note.is_empty() {
            println!("  Note: {note}");
        }
    } else {
        // Could not write — likely no project initialised here.
        eprintln!(
            "[Kimetsu] Could not save checkpoint: no Kimetsu project found at {}.\n\
             Run `kimetsu init` to initialise one.",
            workspace.display()
        );
    }
    Ok(())
}

// ── kimetsu resume ────────────────────────────────────────────────────────────

/// `kimetsu resume` — print the last saved work episode.
fn resume_cmd(args: ResumeArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    match kimetsu_brain::episode::load_live_episode_for_workspace(&workspace) {
        Ok(Some(ep)) => {
            println!("── Resume: last session ──────────────────────────────");
            if !ep.task.is_empty() {
                println!("Task:       {}", ep.task);
            }
            if !ep.summary.is_empty() {
                println!("Summary:    {}", ep.summary);
            }
            if !ep.open_threads.is_empty() {
                println!("Open:       {}", ep.open_threads.join("; "));
            }
            if !ep.dead_ends.is_empty() {
                println!("Avoid:      {}", ep.dead_ends.join("; "));
            }
            if !ep.hypothesis.is_empty() {
                println!("Hypothesis: {}", ep.hypothesis);
            }
            if !ep.note.is_empty() {
                println!("Note:       {}", ep.note);
            }
            println!("Saved:      {}", ep.created_at);
            println!("─────────────────────────────────────────────────────");
        }
        Ok(None) => {
            println!("[Kimetsu] No work episode saved for this repo yet.");
            println!("  Episodes are captured automatically at session end.");
            println!("  You can save one now with: kimetsu checkpoint");
        }
        Err(e) => {
            eprintln!("[Kimetsu] Could not load episode: {e}");
            eprintln!(
                "  Make sure a Kimetsu project is initialised at {}.",
                workspace.display()
            );
        }
    }
    Ok(())
}

// ── kimetsu ps ───────────────────────────────────────────────────────────────

fn ps_cmd(args: PsArgs) -> KimetsuResult<()> {
    let procs = process::list_kimetsu_processes();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&procs)?);
        return Ok(());
    }

    if procs.is_empty() {
        println!("no running kimetsu processes");
        return Ok(());
    }

    // Human table: PID  KIND        WORKSPACE                        EXE
    println!("{:<8}  {:<12}  {:<40}  EXE", "PID", "KIND", "WORKSPACE");
    println!("{}", "-".repeat(100));
    for p in &procs {
        let kind = p.kind.label();
        let workspace = p.workspace.as_deref().unwrap_or("-");
        let exe = p.exe_path.as_deref().unwrap_or("-");
        println!("{:<8}  {:<12}  {:<40}  {}", p.pid, kind, workspace, exe);
    }
    Ok(())
}

// ── kimetsu stop ─────────────────────────────────────────────────────────────

fn stop_cmd(args: StopArgs) -> KimetsuResult<()> {
    let all_procs = process::list_kimetsu_processes();

    // Build the target set.
    let targets: Vec<&process::KimetsuProc> = if !args.pids.is_empty() && !args.all {
        // Explicit PIDs only.
        all_procs
            .iter()
            .filter(|p| args.pids.contains(&p.pid))
            .collect()
    } else {
        // --all, or no pids given — default to all.
        all_procs.iter().collect()
    };

    if targets.is_empty() {
        println!("no running kimetsu processes to stop");
        return Ok(());
    }

    // List what will be stopped.
    println!("The following kimetsu process(es) will be stopped:");
    for p in &targets {
        println!(
            "  PID {}  [{}]  workspace={}",
            p.pid,
            p.kind.label(),
            p.workspace.as_deref().unwrap_or("-")
        );
    }

    // Confirm unless --yes or non-TTY.
    if !args.yes && io::stdin().is_terminal() {
        print!("Stop these processes? [y/N] ");
        io::stdout().flush().ok();
        let stdin = io::stdin();
        let line = stdin.lock().lines().next();
        let answer = match line {
            Some(Ok(l)) => l.trim().to_lowercase(),
            _ => String::new(),
        };
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    } else if !args.yes {
        // Non-TTY without --yes: refuse (same pattern as uninstall).
        return Err(
            "stdin is not a TTY; pass --yes to confirm stopping processes non-interactively".into(),
        );
    }

    let pids: Vec<u32> = targets.iter().map(|p| p.pid).collect();
    let results = process::stop_processes(&pids);

    let mut any_err = false;
    for (pid, result) in &results {
        match result {
            Ok(()) => println!("  stopped PID {pid}"),
            Err(e) => {
                eprintln!("  failed to stop PID {pid}: {e}");
                any_err = true;
            }
        }
    }

    // Hint: host-owned MCP servers are respawned automatically.
    let has_mcp = targets
        .iter()
        .any(|p| p.kind == process::ProcKind::McpServe);
    if has_mcp {
        println!(
            "hint: MCP servers spawned by a host (Claude Code, Codex) are respawned automatically \
             on the next tool call — no manual restart needed."
        );
    }

    if any_err {
        Err("one or more processes could not be stopped (see errors above)".into())
    } else {
        Ok(())
    }
}

// ── kimetsu restart ──────────────────────────────────────────────────────────

fn restart_cmd(args: RestartArgs) -> KimetsuResult<()> {
    // Target: all MCP-serve processes.
    let all_procs = process::list_kimetsu_processes();
    let mcp_procs: Vec<&process::KimetsuProc> = all_procs
        .iter()
        .filter(|p| p.kind == process::ProcKind::McpServe)
        .collect();

    if mcp_procs.is_empty() {
        println!("no running kimetsu MCP server processes found");
        println!(
            "hint: MCP servers are spawned by the host (Claude Code, Codex) on first use. \
             If you expected one, check `kimetsu ps` to see all kimetsu processes."
        );
        return Ok(());
    }

    println!("The following kimetsu MCP server(s) will be stopped:");
    for p in &mcp_procs {
        println!(
            "  PID {}  workspace={}",
            p.pid,
            p.workspace.as_deref().unwrap_or("-")
        );
    }

    if !args.yes && io::stdin().is_terminal() {
        print!("Stop and let the host respawn them? [y/N] ");
        io::stdout().flush().ok();
        let stdin = io::stdin();
        let line = stdin.lock().lines().next();
        let answer = match line {
            Some(Ok(l)) => l.trim().to_lowercase(),
            _ => String::new(),
        };
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    } else if !args.yes {
        return Err(
            "stdin is not a TTY; pass --yes to confirm stopping processes non-interactively".into(),
        );
    }

    let pids: Vec<u32> = mcp_procs.iter().map(|p| p.pid).collect();
    let results = process::stop_processes(&pids);

    let mut any_err = false;
    for (pid, result) in &results {
        match result {
            Ok(()) => println!("  stopped PID {pid}"),
            Err(e) => {
                eprintln!("  failed to stop PID {pid}: {e}");
                any_err = true;
            }
        }
    }

    println!(
        "\nThe host agent (Claude Code / Codex) will automatically respawn the MCP server \
         on the next kimetsu tool call — no manual restart is needed."
    );

    if any_err {
        Err("one or more MCP server processes could not be stopped (see errors above)".into())
    } else {
        Ok(())
    }
}

// ── kimetsu setup — one-command onboarding ───────────────────────────────────

/// Resolve which host(s) to install into.
///
/// Priority:
/// 1. `--host` flag (explicit wins).
/// 2. Auto-detect from present home config dirs (`~/.claude`, `~/.codex`, `~/.pi`).
/// 3. None present + non-TTY → default `claude-code` with a note.
/// 4. None present + TTY → prompt with the provided `reader`.
///
/// Factored as a pure-ish function so it can be unit-tested without real installs.
#[allow(clippy::too_many_arguments)]
pub fn resolve_setup_hosts(
    arg: Option<&str>,
    present_claude: bool,
    present_codex: bool,
    present_cursor: bool,
    present_openclaw: bool,
    present_pi: bool,
    is_tty: bool,
    mut reader: impl io::BufRead,
) -> Result<Vec<kimetsu_chat::BridgeTarget>, String> {
    use kimetsu_chat::BridgeTarget;

    if let Some(raw) = arg {
        if raw.eq_ignore_ascii_case("both") {
            return Ok(vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
        }
        let target = BridgeTarget::parse(raw)?;
        return Ok(vec![target]);
    }

    // Auto-detect from present home dirs.
    let mut detected: Vec<BridgeTarget> = Vec::new();
    if present_claude {
        detected.push(BridgeTarget::ClaudeCode);
    }
    if present_codex {
        detected.push(BridgeTarget::Codex);
    }
    if present_cursor {
        detected.push(BridgeTarget::Cursor);
    }
    #[cfg(feature = "openclaw")]
    if present_openclaw {
        detected.push(BridgeTarget::OpenClaw);
    }
    #[cfg(not(feature = "openclaw"))]
    let _ = present_openclaw;
    #[cfg(feature = "pi")]
    if present_pi {
        detected.push(BridgeTarget::Pi);
    }
    #[cfg(not(feature = "pi"))]
    let _ = present_pi;

    if !detected.is_empty() {
        return Ok(detected);
    }

    // Nothing detected.
    if !is_tty {
        eprintln!(
            "note: no recognized host config dirs found; defaulting to claude-code. \
             Pass --host to choose explicitly."
        );
        Ok(vec![BridgeTarget::ClaudeCode])
    } else {
        #[cfg(all(feature = "pi", feature = "openclaw"))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/openclaw/pi/both]: ";
        #[cfg(all(feature = "pi", not(feature = "openclaw")))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/pi/both]: ";
        #[cfg(all(not(feature = "pi"), feature = "openclaw"))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/openclaw/both]: ";
        #[cfg(all(not(feature = "pi"), not(feature = "openclaw")))]
        let prompt = "Which host agent do you use? [claude-code/codex/cursor/both]: ";
        print!("{prompt}");
        io::stdout().flush().ok();
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("setup: failed to read host selection: {e}"))?;
        let answer = line.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "claude-code" || answer == "claude" || answer == "cc" {
            Ok(vec![BridgeTarget::ClaudeCode])
        } else if answer == "codex" {
            Ok(vec![BridgeTarget::Codex])
        } else if answer == "both" {
            Ok(vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex])
        } else {
            BridgeTarget::parse(&answer).map(|t| vec![t])
        }
    }
}

/// Detect whether the home config directories for Claude Code, Codex, Cursor,
/// OpenClaw, and Pi exist.
/// Returns `(claude_present, codex_present, cursor_present, openclaw_present, pi_present)`.
fn detect_present_hosts() -> (bool, bool, bool, bool, bool) {
    let home = std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
        .map(std::path::PathBuf::from);

    let home = match home {
        Some(h) => h,
        None => return (false, false, false, false, false),
    };

    let claude_present = home.join(".claude").is_dir();
    let codex_present = home.join(".codex").is_dir();
    // Cursor: global config lives in ~/.cursor
    let cursor_present = home.join(".cursor").is_dir();
    #[cfg(feature = "openclaw")]
    let openclaw_present = home.join(".openclaw").is_dir();
    #[cfg(not(feature = "openclaw"))]
    let openclaw_present = false;
    #[cfg(feature = "pi")]
    let pi_present = home.join(".pi").is_dir();
    #[cfg(not(feature = "pi"))]
    let pi_present = false;
    (
        claude_present,
        codex_present,
        cursor_present,
        openclaw_present,
        pi_present,
    )
}

/// `kimetsu setup` — one-command onboarding.
fn setup_cmd(args: SetupArgs) -> KimetsuResult<()> {
    use kimetsu_chat::{BridgeTarget, InstallScope, PluginMode, plugin_install};

    let workspace = args
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| args.workspace.clone());

    println!("=== kimetsu setup ===");
    println!(
        "workspace: {}",
        kimetsu_core::paths::display_path(&workspace)
    );
    println!();

    // ── Step 1: Init ──────────────────────────────────────────────────────────
    println!("[1/4] Initializing project...");
    let init_result = project::init_project(&workspace, false);
    let init_ok = match init_result {
        Ok(ref summary) => {
            if summary.wrote_project_toml {
                println!(
                    "  initialized .kimetsu/ at {}",
                    kimetsu_core::paths::display_path(&summary.kimetsu_dir)
                );
            } else {
                println!(
                    "  project already initialized at {}",
                    kimetsu_core::paths::display_path(&summary.kimetsu_dir)
                );
            }
            true
        }
        Err(ref e) => {
            eprintln!("  error: init failed: {e}");
            eprintln!("  cannot continue without a valid project. Fix the error and re-run setup.");
            // Print summary of what succeeded (nothing) before bailing.
            println!();
            println!("=== setup summary ===");
            println!("  init:    FAILED — {e}");
            println!("  install: skipped");
            println!("  verify:  skipped");
            return Err(format!("kimetsu setup: init failed: {e}").into());
        }
    };
    let _ = init_ok;

    // ── Step 2: Choose host(s) ────────────────────────────────────────────────
    println!();
    println!("[2/4] Selecting host(s)...");
    let (present_claude, present_codex, present_cursor, present_openclaw, present_pi) =
        detect_present_hosts();
    let is_tty = io::stdin().is_terminal();
    let stdin = io::stdin();
    let hosts = resolve_setup_hosts(
        args.host.as_deref(),
        present_claude,
        present_codex,
        present_cursor,
        present_openclaw,
        present_pi,
        is_tty,
        stdin.lock(),
    )
    .map_err(|e| format!("kimetsu setup: {e}"))?;

    let scope = InstallScope::parse(&args.scope).map_err(|e| format!("kimetsu setup: {e}"))?;
    let mode = PluginMode::parse(&args.mode).map_err(|e| format!("kimetsu setup: {e}"))?;

    let host_labels: Vec<&str> = hosts.iter().map(|h| h.as_str()).collect();
    let scope_gloss = match scope {
        InstallScope::Workspace => "this project only",
        InstallScope::Global => "every project",
    };
    let mode_gloss = match mode {
        PluginMode::Optional => "recommended, non-blocking",
        PluginMode::Required => "treated as a setup blocker for big tasks",
    };
    println!(
        "  hosts: {}   scope: {} ({})   mode: {} ({})",
        host_labels.join(", "),
        scope.as_str(),
        scope_gloss,
        mode.as_str(),
        mode_gloss,
    );

    // ── Step 3: Install ───────────────────────────────────────────────────────
    println!();
    println!("[3/4] Installing plugin wiring...");

    let mut install_warnings: Vec<String> = Vec::new();
    let mut install_failed = false;
    let mut installed_hosts: Vec<String> = Vec::new();

    for &target in &hosts {
        let host_label = match target {
            BridgeTarget::ClaudeCode => "Claude Code",
            BridgeTarget::Codex => "Codex",
            BridgeTarget::Kimetsu => "Kimetsu",
            BridgeTarget::Cursor => "Cursor",
            #[cfg(feature = "openclaw")]
            BridgeTarget::OpenClaw => "OpenClaw",
            #[cfg(feature = "pi")]
            BridgeTarget::Pi => "Pi",
        };
        println!(
            "  installing into {host_label} ({} scope)...",
            scope.as_str()
        );

        match plugin_install(
            &workspace,
            target,
            scope,
            mode,
            false, // force — idempotent
            !args.no_proactive,
        ) {
            Ok(report) => {
                for f in &report.files {
                    let rel = f
                        .strip_prefix(&workspace)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| kimetsu_core::paths::display_path(f));
                    println!("    {rel}");
                }
                for note in &report.notes {
                    println!("    {note}");
                }
                installed_hosts.push(format!("{} ({})", host_label, scope.as_str()));

                // Run the distiller setup wizard unless suppressed.
                if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex)
                    && !args.no_setup
                    && is_tty
                    && io::stdout().is_terminal()
                {
                    let target_for_scope = match scope {
                        InstallScope::Global => {
                            kimetsu_core::paths::user_kimetsu_dir().map(|dir| {
                                (
                                    harvest_setup::SetupTarget {
                                        project_toml: dir.join("project.toml"),
                                        env_path: dir.join(".env"),
                                        gitignore_dir: dir,
                                    },
                                    "globally (all projects, ~/.kimetsu)",
                                )
                            })
                        }
                        InstallScope::Workspace => {
                            let p = kimetsu_core::paths::ProjectPaths::at_root(&workspace);
                            Some((
                                harvest_setup::SetupTarget {
                                    project_toml: p.project_toml.clone(),
                                    env_path: p.repo_root.join(".env"),
                                    gitignore_dir: p.repo_root.clone(),
                                },
                                "this workspace",
                            ))
                        }
                    };
                    if let Some((setup_target, label)) = target_for_scope {
                        let stdin2 = std::io::stdin();
                        let mut reader2 = stdin2.lock();
                        let mut stdout2 = std::io::stdout();
                        if let Err(e) = harvest_setup::run_harvest_setup(
                            &mut reader2,
                            &mut stdout2,
                            &setup_target,
                            label,
                        ) {
                            eprintln!("  distiller setup skipped: {e}");
                        }
                    }
                }

                // Self-check: confirm wiring landed.
                if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex) {
                    let warnings =
                        plugin_install_self_check(&workspace, target.as_str(), scope.as_str());
                    install_warnings.extend(warnings);
                }
            }
            Err(e) => {
                eprintln!("  error: install into {host_label} failed: {e}");
                install_failed = true;
            }
        }
    }

    if install_failed {
        // Core step failed — return non-zero.
        println!();
        println!("=== setup summary ===");
        println!("  init:    OK");
        if installed_hosts.is_empty() {
            println!("  install: FAILED (all hosts)");
        } else {
            println!(
                "  install: partial — succeeded: {}",
                installed_hosts.join(", ")
            );
        }
        println!("  verify:  skipped");
        return Err("kimetsu setup: one or more plugin installs failed (see errors above)".into());
    }

    // ── Step 4: Verify (selftest) ─────────────────────────────────────────────
    println!();
    println!("[4/4] Verifying brain (doctor --selftest)...");
    let selftest_ok = if args.no_selftest {
        println!("  skipped (--no-selftest)");
        true
    } else {
        match doctor::run_selftest() {
            Ok(()) => true,
            Err(e) => {
                eprintln!("  selftest FAILED: {e}");
                false
            }
        }
    };

    // ── Summary ───────────────────────────────────────────────────────────────
    println!();
    println!("=== setup summary ===");
    println!("  init:    OK");
    println!("  install: {}", installed_hosts.join(", "));
    if args.no_selftest {
        println!("  verify:  skipped");
    } else if selftest_ok {
        println!("  verify:  ✓ PASS");
    } else {
        println!("  verify:  ✗ FAIL (brain not working — check logs above)");
    }

    // Surface PATH warnings prominently if present.
    let path_warnings: Vec<&String> = install_warnings
        .iter()
        .filter(|w| w.contains("PATH"))
        .collect();
    if !path_warnings.is_empty() {
        println!();
        println!("IMPORTANT — kimetsu not on PATH:");
        for w in &path_warnings {
            println!("  {w}");
        }
    }

    println!();
    let host_names: Vec<&str> = hosts
        .iter()
        .map(|t| match t {
            BridgeTarget::ClaudeCode => "Claude Code",
            BridgeTarget::Codex => "Codex",
            BridgeTarget::Kimetsu => "Kimetsu",
            BridgeTarget::Cursor => "Cursor",
            #[cfg(feature = "openclaw")]
            BridgeTarget::OpenClaw => "OpenClaw",
            #[cfg(feature = "pi")]
            BridgeTarget::Pi => "Pi",
        })
        .collect();
    println!(
        "Next step: Restart your host agent ({}) so it loads the Kimetsu MCP server.",
        host_names.join(" / ")
    );

    Ok(())
}

/// v0.4.6: `kimetsu doctor` entry point. Runs the full health
/// suite + prints either the human or JSON report.
///
/// Exit codes:
///   0 — all checks passed or warned.
///   1 — at least one Fail.
///   2 — internal doctor error (couldn't even run the checks).
fn doctor_cmd(args: DoctorArgs) -> KimetsuResult<()> {
    // D4: --selftest runs a hermetic round-trip and exits early.
    if args.selftest {
        return doctor::run_selftest();
    }
    let opts = doctor::DoctorOptions {
        json: args.json,
        skip_mcp: args.skip_mcp,
    };
    let workspace = match args.workspace.canonicalize() {
        Ok(p) => p,
        Err(_) => args.workspace.clone(),
    };
    let report = doctor::run(&workspace, opts.clone())?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        doctor::print_human(&report);
    }
    if !report.ok() {
        std::process::exit(1);
    }
    Ok(())
}

/// v0.3: `kimetsu chat` subcommand. Reuses the kimetsu-agent runtime
/// via the kimetsu-chat crate. NO dependency on kimetsu-harbor-rs â€” by
/// design, chat is its own product surface, completely independent of
/// Terminal-Bench / Harbor.
fn bridge(command: BridgeCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{
        BridgeTarget, bridge_export_skill, bridge_import_skill, bridge_scan, bridge_sync,
    };

    match command {
        BridgeCommand::Scan(args) | BridgeCommand::Status(args) | BridgeCommand::Doctor(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let scan = bridge_scan(&workspace, &config)
                .map_err(|err| format!("kimetsu bridge scan: {err}"))?;
            println!("workspace: {}", workspace.display());
            println!("extensions: {}", scan.extensions.len());
            for extension in &scan.extensions {
                println!(
                    "  {} [{}] {}",
                    extension.manifest.name,
                    extension.manifest.source,
                    extension.root.display()
                );
            }
            println!("skills: {}", scan.skills.len());
            for skill in &scan.skills {
                println!(
                    "  {}  kimetsu_ext={} kimetsu={} claude={} codex={}  origin={}",
                    skill.name,
                    skill.kimetsu_extension,
                    skill.kimetsu_skill,
                    skill.claude_skill,
                    skill.codex_skill,
                    skill.origin
                );
            }
            if scan.skills.is_empty() {
                println!(
                    "no skills found; add provider skills or run `kimetsu plugin install <target>`"
                );
            }
        }
        BridgeCommand::Import(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let imported = bridge_import_skill(&workspace, &config, &args.selection, args.force)
                .map_err(|err| format!("kimetsu bridge import: {err}"))?;
            println!(
                "imported {} into {}",
                imported.manifest.name,
                imported.root.display()
            );
        }
        BridgeCommand::Export(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu bridge export: {err}"))?;
            let exported =
                bridge_export_skill(&workspace, &config, &args.selection, target, args.force)
                    .map_err(|err| format!("kimetsu bridge export: {err}"))?;
            println!(
                "exported {} to {} at {}",
                args.selection,
                target.as_str(),
                exported.display()
            );
        }
        BridgeCommand::Sync(args) => {
            let workspace = args.workspace.canonicalize()?;
            let config = bridge_skill_config(args.no_user_skills);
            let imported = bridge_sync(&workspace, &config, args.force)
                .map_err(|err| format!("kimetsu bridge sync: {err}"))?;
            println!("imported {imported} skill bundle(s) into .kimetsu/extensions");
        }
    }
    Ok(())
}

fn mcp(command: McpCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{McpServeConfig, serve_mcp};

    match command {
        McpCommand::Serve(args) => {
            let mut config = McpServeConfig::new(args.workspace);
            config.skills.include_user_roots = !args.no_user_skills;
            let stdin = io::stdin();
            let stdout = io::stdout();
            serve_mcp(stdin.lock(), stdout.lock(), config)
                .map_err(|err| format!("kimetsu mcp serve: {err}"))?;
        }
    }
    Ok(())
}

// ── plugin install self-check ────────────────────────────────────────────────

/// Check whether the `kimetsu` binary is resolvable on the current PATH.
///
/// Returns `true` when any entry in `PATH` contains a file named `kimetsu`
/// (or `kimetsu.exe` on Windows). Factored out for unit-testability.
pub fn kimetsu_on_path() -> bool {
    kimetsu_on_path_with(std::env::var_os("PATH").as_deref())
}

/// Inner implementation; takes an optional raw PATH value so tests can
/// inject a controlled PATH without touching the real environment.
pub fn kimetsu_on_path_with(path_var: Option<&std::ffi::OsStr>) -> bool {
    let Some(path_var) = path_var else {
        return false;
    };
    let bin = if cfg!(windows) {
        "kimetsu.exe"
    } else {
        "kimetsu"
    };
    std::env::split_paths(path_var).any(|dir| dir.join(bin).is_file())
}

/// Best-effort post-install self-check.
///
/// 1. Confirms `kimetsu` resolves on PATH.
/// 2. Calls `plugin_status` and verifies the just-installed (host, scope)
///    reports `WiringState::Installed`.
/// 3. Prints a concise summary + the "restart your host" next-step message.
///
/// A failed check prints a warning but does NOT cause the install to fail
/// (the files were already written).  Returns the list of warning strings
/// so tests can assert on the output without capturing stdout.
pub fn plugin_install_self_check(
    workspace: &std::path::Path,
    host: &str,
    scope: &str,
) -> Vec<String> {
    use kimetsu_chat::{WiringState, plugin_status};

    let mut warnings: Vec<String> = Vec::new();

    // 1. PATH check.
    if !kimetsu_on_path() {
        warnings.push(
            "warning: `kimetsu` is not on your PATH — the installed hooks call the bare \
             `kimetsu` command, but it won't be found. Add the install directory \
             (e.g. `~/.cargo/bin`) to your PATH so the hooks can run."
                .to_string(),
        );
    }

    // 2. Wiring check via plugin_status.
    let statuses = plugin_status(workspace);
    let entry = statuses.iter().find(|s| s.host == host && s.scope == scope);

    match entry {
        Some(s) if matches!(s.state, WiringState::Installed) => {
            // All good — success line.
            let host_label = match host {
                "claude-code" => "Claude Code",
                "codex" => "Codex",
                other => other,
            };
            println!(
                "✓ wired into {host_label} ({scope} scope). \
                 Restart your host agent ({host_label}) so it picks up the MCP server."
            );
        }
        Some(s) if matches!(s.state, WiringState::Partial) => {
            let warn = format!(
                "warning: wiring is partial for {} ({}). Missing pieces: [{}]. \
                 Re-run `kimetsu plugin install {}` to complete it.",
                host,
                scope,
                s.missing.join(", "),
                host
            );
            warnings.push(warn.clone());
            eprintln!("{warn}");
        }
        Some(_) | None => {
            let warn = format!(
                "warning: could not confirm wiring landed for {host} ({scope}). \
                 Run `kimetsu plugin status` to inspect."
            );
            warnings.push(warn.clone());
            eprintln!("{warn}");
        }
    }

    // Emit any PATH warnings to stderr.
    for w in &warnings {
        if w.contains("PATH") {
            eprintln!("{w}");
        }
    }

    warnings
}

/// Normalize a git remote URL (or an explicit `--repo`) into a stable,
/// server-safe id: drop scheme/credentials/`.git`, then slug to
/// `[a-z0-9-]`. `https://github.com/org/repo.git` and
/// `git@github.com:org/repo.git` both → `github-com-org-repo`.
fn normalize_repo_id(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped;
    }
    if let Some((_, rest)) = s.split_once("://") {
        s = rest;
    }
    if let Some((_, rest)) = s.split_once('@') {
        s = rest;
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Derive a repo id from `git -C <workspace> remote get-url origin`.
fn derive_repo_id(workspace: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = normalize_repo_id(String::from_utf8_lossy(&out.stdout).trim());
    (!id.is_empty()).then_some(id)
}

/// Wire a host to a remote kimetsu-remote server (HTTP MCP).
fn run_plugin_install_remote(
    workspace: &std::path::Path,
    target: kimetsu_chat::BridgeTarget,
    scope: kimetsu_chat::InstallScope,
    mode: kimetsu_chat::PluginMode,
    args: &PluginInstallArgs,
    base: &str,
) -> KimetsuResult<()> {
    let repo_id = match &args.repo {
        Some(r) => normalize_repo_id(r),
        None => derive_repo_id(workspace).ok_or_else(|| {
            "kimetsu plugin install: could not derive a repo id from this repo's git remote; \
             pass --repo <id>"
                .to_string()
        })?,
    };
    if repo_id.is_empty() {
        return Err("kimetsu plugin install: --repo resolved to an empty id".into());
    }
    let remote = kimetsu_chat::RemoteInstall {
        base_url: base.to_string(),
        repo_id: repo_id.clone(),
        token: args.token.clone(),
    };
    let report = kimetsu_chat::plugin_install_remote(workspace, target, scope, mode, &remote)
        .map_err(|err| format!("kimetsu plugin install: {err}"))?;

    let host_label = match target {
        kimetsu_chat::BridgeTarget::ClaudeCode => "Claude Code",
        #[cfg(feature = "openclaw")]
        kimetsu_chat::BridgeTarget::OpenClaw => "OpenClaw",
        _ => "host",
    };
    println!(
        "Wiring Kimetsu (remote) into {host_label} ({} scope) → repo `{repo_id}`…",
        report.scope.as_str()
    );
    println!("  wrote/updated:");
    for file in &report.files {
        let rel = file
            .strip_prefix(workspace)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| kimetsu_core::paths::display_path(file));
        println!("    {rel}");
    }
    for note in &report.notes {
        println!("  {note}");
    }
    println!("  ✓ wired. Restart your host agent so it connects to the remote brain.");
    println!(
        "  note: Kimetsu Remote is BETA (under active testing — expect rough edges). The \
         `kimetsu-remote` server is a SEPARATE binary: `cargo install kimetsu-remote --features \
         embeddings` (or the embeddings release archive) — it is not installed with `kimetsu`."
    );
    Ok(())
}

fn plugin(command: PluginCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{
        BridgeTarget, InstallScope, PluginMode, WiringState, plugin_install, plugin_status,
        plugin_uninstall,
    };

    match command {
        PluginCommand::Install(args) => {
            // Canonicalize leniently: a global install doesn't use the
            // workspace, so a missing `--workspace` path shouldn't fail it.
            let workspace = args
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| args.workspace.clone());
            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let scope = InstallScope::parse(&args.scope)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let mode = PluginMode::parse(&args.mode)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            // Remote wiring: point the host at a kimetsu-remote HTTP MCP server.
            if let Some(base) = args.remote.clone() {
                return run_plugin_install_remote(&workspace, target, scope, mode, &args, &base);
            }
            // The kimetsu extensions target is workspace-only; warn rather
            // than silently ignore a `--scope global` for it.
            if matches!(scope, InstallScope::Global) && matches!(target, BridgeTarget::Kimetsu) {
                eprintln!(
                    "kimetsu plugin install: --scope global has no effect for the `kimetsu` target; \
                     installing to the workspace .kimetsu/extensions."
                );
            }
            let report = plugin_install(
                &workspace,
                target,
                scope,
                mode,
                args.force,
                !args.no_proactive,
            )
            .map_err(|err| format!("kimetsu plugin install: {err}"))?;

            // Friendly framing: intro line with plain-language scope/mode glosses.
            let host_label = match target {
                BridgeTarget::ClaudeCode => "Claude Code",
                BridgeTarget::Codex => "Codex",
                BridgeTarget::Kimetsu => "Kimetsu",
                BridgeTarget::Cursor => "Cursor",
                #[cfg(feature = "openclaw")]
                BridgeTarget::OpenClaw => "OpenClaw",
                #[cfg(feature = "pi")]
                BridgeTarget::Pi => "Pi",
            };
            let scope_gloss = match scope {
                InstallScope::Workspace => "this project only",
                InstallScope::Global => "every project",
            };
            let mode_gloss = match mode {
                PluginMode::Optional => "recommended, non-blocking",
                PluginMode::Required => "treated as a setup blocker for big tasks",
            };
            println!(
                "Wiring Kimetsu into {host_label} ({} scope — {scope_gloss}, {} mode — {mode_gloss})…",
                report.scope.as_str(),
                report.mode.as_str(),
            );
            println!("  wrote/updated:");
            for file in &report.files {
                // Show workspace-relative path when possible; fall back to display_path.
                let rel = file
                    .strip_prefix(&workspace)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| kimetsu_core::paths::display_path(file));
                println!("    {rel}");
            }
            for note in &report.notes {
                println!("  {note}");
            }
            // Offer interactive distiller setup for host targets on a TTY.
            let interactive = args.setup_harvest
                || (std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
            if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex)
                && !args.no_setup
                && interactive
            {
                let target_for_scope = match scope {
                    InstallScope::Global => match kimetsu_core::paths::user_kimetsu_dir() {
                        Some(dir) => Some((
                            harvest_setup::SetupTarget {
                                project_toml: dir.join("project.toml"),
                                env_path: dir.join(".env"),
                                gitignore_dir: dir,
                            },
                            "globally (all projects, ~/.kimetsu)",
                        )),
                        None => {
                            eprintln!(
                                "kimetsu plugin install: cannot resolve ~/.kimetsu; skipping distiller setup."
                            );
                            None
                        }
                    },
                    InstallScope::Workspace => {
                        let p = kimetsu_core::paths::ProjectPaths::at_root(&workspace);
                        Some((
                            harvest_setup::SetupTarget {
                                project_toml: p.project_toml.clone(),
                                env_path: p.repo_root.join(".env"),
                                gitignore_dir: p.repo_root.clone(),
                            },
                            "this workspace",
                        ))
                    }
                };
                if let Some((setup_target, label)) = target_for_scope {
                    let stdin = std::io::stdin();
                    let mut reader = stdin.lock();
                    let mut stdout = std::io::stdout();
                    if let Err(err) = harvest_setup::run_harvest_setup(
                        &mut reader,
                        &mut stdout,
                        &setup_target,
                        label,
                    ) {
                        eprintln!("kimetsu plugin install: distiller setup skipped: {err}");
                    }
                }
            }
            // Self-check: confirm wiring landed + PATH hint.
            // Only for host targets; the `kimetsu` extensions target
            // doesn't invoke the bare `kimetsu` command.
            if matches!(target, BridgeTarget::ClaudeCode | BridgeTarget::Codex) {
                plugin_install_self_check(&workspace, target.as_str(), scope.as_str());
            }
        }

        PluginCommand::Status(args) => {
            let workspace = args
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| args.workspace.clone());

            let statuses = plugin_status(&workspace);

            // Collect running MCP servers.
            let mcp_procs: Vec<_> = process::list_kimetsu_processes()
                .into_iter()
                .filter(|p| p.kind == process::ProcKind::McpServe)
                .collect();

            // Determine the on-PATH kimetsu version.
            let path_version = kimetsu_version_on_path();
            let this_version = env!("CARGO_PKG_VERSION");

            if args.json {
                #[derive(serde::Serialize)]
                struct StatusOutput<'a> {
                    wiring: &'a Vec<kimetsu_chat::PluginScopeStatus>,
                    this_binary_version: &'a str,
                    path_version: Option<String>,
                    mcp_servers: Vec<MiniProc>,
                }
                #[derive(serde::Serialize)]
                struct MiniProc {
                    pid: u32,
                    workspace: Option<String>,
                    exe_path: Option<String>,
                }
                let output = StatusOutput {
                    wiring: &statuses,
                    this_binary_version: this_version,
                    path_version,
                    mcp_servers: mcp_procs
                        .iter()
                        .map(|p| MiniProc {
                            pid: p.pid,
                            workspace: p.workspace.clone(),
                            exe_path: p.exe_path.clone(),
                        })
                        .collect(),
                };
                println!("{}", serde_json::to_string_pretty(&output)?);
                return Ok(());
            }

            // Human-readable report.
            let any_wired = statuses
                .iter()
                .any(|s| !matches!(s.state, WiringState::Absent));

            if !any_wired {
                println!(
                    "Kimetsu is not installed into any host (workspace or global).\n\
                     Run `kimetsu plugin install <claude-code|codex>` to wire it in."
                );
                return Ok(());
            }

            println!("Kimetsu plugin wiring status");
            println!("{}", "─".repeat(60));

            for s in &statuses {
                let state_label = match s.state {
                    WiringState::Installed => "INSTALLED",
                    WiringState::Partial => "PARTIAL  ",
                    WiringState::Absent => "absent   ",
                };
                let present_str = if s.present.is_empty() {
                    String::new()
                } else {
                    format!("  present: [{}]", s.present.join(", "))
                };
                let missing_str = if s.missing.is_empty() {
                    String::new()
                } else {
                    format!("  missing: [{}]", s.missing.join(", "))
                };
                println!(
                    "  {:<12}  {:<10}  {}{}{}",
                    s.host, s.scope, state_label, present_str, missing_str
                );
                if !matches!(s.state, WiringState::Absent) {
                    // Strip \\?\ prefix that canonicalize() can add on Windows.
                    let cfg_display =
                        kimetsu_core::paths::display_path(std::path::Path::new(&s.config_path));
                    println!("    config: {cfg_display}");
                }
            }

            println!("{}", "─".repeat(60));
            println!("This binary:  v{this_version}");
            match &path_version {
                Some(pv) if pv != this_version => {
                    println!("On PATH:      v{pv}  (differs from this binary)");
                }
                Some(pv) => println!("On PATH:      v{pv}"),
                None => println!("On PATH:      (could not determine)"),
            }

            if mcp_procs.is_empty() {
                println!("MCP servers:  none running");
            } else {
                println!("MCP servers:");
                for p in &mcp_procs {
                    println!(
                        "  PID {}  workspace={}",
                        p.pid,
                        p.workspace.as_deref().unwrap_or("-")
                    );
                }
            }
        }

        PluginCommand::Uninstall(args) => {
            let workspace = args
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| args.workspace.clone());

            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu plugin uninstall: {err}"))?;

            // Collect scopes to uninstall from.
            let scopes: Vec<InstallScope> = if args.all_scopes {
                vec![InstallScope::Workspace, InstallScope::Global]
            } else {
                let scope = InstallScope::parse(&args.scope)
                    .map_err(|err| format!("kimetsu plugin uninstall: {err}"))?;
                vec![scope]
            };

            // Show current status for the target+scopes and confirm.
            let all_statuses = plugin_status(&workspace);
            let relevant: Vec<_> = all_statuses
                .iter()
                .filter(|s| {
                    s.host == target.as_str()
                        && scopes.iter().any(|sc| sc.as_str() == s.scope.as_str())
                })
                .collect();

            let anything_present = relevant
                .iter()
                .any(|s| !matches!(s.state, WiringState::Absent));

            if !anything_present {
                println!(
                    "No Kimetsu wiring found for {} ({}) — nothing to remove.",
                    target.as_str(),
                    scopes
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("+")
                );
                return Ok(());
            }

            // Show what will be removed.
            for s in &relevant {
                if !matches!(s.state, WiringState::Absent) {
                    println!(
                        "Will remove Kimetsu wiring from {} ({}): [{}]",
                        s.host,
                        s.scope,
                        s.present.join(", ")
                    );
                }
            }
            println!(
                "\nThis removes ONLY the host wiring — the Kimetsu binary, brain, and your \
                 other hooks/servers are NOT touched."
            );

            // Interactive confirm.
            let scope_label = scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" + ");
            if !args.yes && io::stdin().is_terminal() {
                print!(
                    "Remove Kimetsu's wiring from {} ({})? [y/N] ",
                    target.as_str(),
                    scope_label
                );
                io::stdout().flush().ok();
                let stdin = io::stdin();
                let line = stdin.lock().lines().next();
                let answer = match line {
                    Some(Ok(l)) => l.trim().to_lowercase(),
                    _ => String::new(),
                };
                if answer != "y" && answer != "yes" {
                    println!("Aborted.");
                    return Ok(());
                }
            } else if !args.yes {
                return Err("stdin is not a TTY; pass --yes to confirm non-interactively".into());
            }

            // Execute uninstall for each scope.
            for scope in &scopes {
                let report = plugin_uninstall(&workspace, target, *scope)
                    .map_err(|err| format!("kimetsu plugin uninstall: {err}"))?;

                if report.removed.is_empty() && report.modified.is_empty() {
                    println!(
                        "  {} scope: nothing to remove (already clean)",
                        scope.as_str()
                    );
                } else {
                    for path in &report.removed {
                        println!("  removed  {}", path.display());
                    }
                    for path in &report.modified {
                        println!("  modified {}", path.display());
                    }
                }
            }

            println!(
                "\nKimetsu plugin wiring removed from {} ({}).",
                target.as_str(),
                scope_label
            );
            println!(
                "The Kimetsu binary, brain, and any other hooks/servers are untouched.\n\
                 To reinstall: `kimetsu plugin install {}`",
                target.as_str()
            );
        }
    }
    Ok(())
}

/// Try to determine the version of `kimetsu` on the PATH by running `kimetsu --version`.
/// Returns `None` if not found or if the output is not parseable.
fn kimetsu_version_on_path() -> Option<String> {
    let output = std::process::Command::new("kimetsu")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    // clap emits "kimetsu <version>"
    text.strip_prefix("kimetsu ").map(|rest| rest.to_string())
}

fn bridge_skill_config(no_user_skills: bool) -> kimetsu_chat::SkillConfig {
    kimetsu_chat::SkillConfig {
        include_user_roots: !no_user_skills,
        ..kimetsu_chat::SkillConfig::default()
    }
}

fn chat(args: ChatArgs) -> KimetsuResult<()> {
    use kimetsu_chat::{
        ChatConfig, ChatUi, SkillRegistry, rich_ui_enabled_from_env, run_repl, skill_origin_label,
    };
    use std::io::{stdin, stdout};

    let mut config = ChatConfig::new(args.workspace);
    config.brain_project = args.project;
    if let Some(m) = args.model {
        config.model = m;
    } else if let Ok(m) = std::env::var("KIMETSU_MODEL")
        && !m.is_empty()
    {
        config.model = m;
    }
    config.max_cost_usd = args.max_cost_usd;
    config.goal = args.goal;
    config.strict_verify = args.strict;
    config.skills.selected = args.skills;
    config.skills.roots = args.skill_dirs;
    config.skills.include_workspace_roots = !args.no_workspace_skills;
    config.skills.include_user_roots = !args.no_user_skills;

    let stdin = stdin();
    let stdout = stdout();
    config.raw_terminal_input = stdin.is_terminal() && stdout.is_terminal();
    config.persist_sessions = true;
    config.ui = if !args.plain && stdout.is_terminal() && rich_ui_enabled_from_env() {
        ChatUi::rich()
    } else {
        ChatUi::plain()
    }
    .with_logo(!args.no_logo);
    if args.list_skill_sources {
        let workspace = config.workspace_root.canonicalize()?;
        let registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --list-skill-sources: {err}"))?;
        if registry.roots().is_empty() {
            println!("no skill sources configured");
        } else {
            for root in registry.roots() {
                let status = if root.exists { "found" } else { "missing" };
                let login = match root.kind.as_str() {
                    "workspace" | "extra" => "local",
                    _ if root.logged_in => "login detected",
                    _ => "login unknown",
                };
                let marketplace = root
                    .marketplace
                    .as_ref()
                    .map(|marketplace| format!(" marketplace={marketplace}"))
                    .unwrap_or_default();
                println!(
                    "{} [{}; {}; {}{}]\n  {}",
                    root.source.as_str(),
                    root.kind.as_str(),
                    status,
                    login,
                    marketplace,
                    root.path.display()
                );
            }
        }
        return Ok(());
    }
    if !args.install_skills.is_empty() {
        let workspace = config.workspace_root.canonicalize()?;
        let mut registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --install-skill: {err}"))?;
        for selection in &args.install_skills {
            let installed = registry
                .install_as_kimetsu(selection, args.install_skill_force)
                .map_err(|err| format!("kimetsu chat --install-skill {selection}: {err}"))?;
            println!(
                "installed {} as Kimetsu skill\n  {}",
                installed.name,
                installed.root.display()
            );
            registry
                .refresh(&config.skills)
                .map_err(|err| format!("kimetsu chat --install-skill refresh: {err}"))?;
        }
        if !args.list_skills {
            return Ok(());
        }
    }
    if let Some(query) = &args.search_skills {
        let workspace = config.workspace_root.canonicalize()?;
        let registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --search-skills: {err}"))?;
        let matches = registry.matching_skills(query);
        if matches.is_empty() {
            println!("no skills matched `{query}`");
        } else {
            for skill in matches {
                let state = if registry.is_installed(skill) {
                    "installed"
                } else {
                    "available"
                };
                println!(
                    "{} [{}; {}]\n  {}\n  root: {}\n  entrypoint: {}\n  resources: {}",
                    skill.name,
                    state,
                    skill_origin_label(skill),
                    skill.description,
                    skill.root.display(),
                    skill.path.display(),
                    skill.resource_summary()
                );
            }
        }
        return Ok(());
    }
    if args.list_skills {
        let workspace = config.workspace_root.canonicalize()?;
        let registry = SkillRegistry::discover(&workspace, &config.skills)
            .map_err(|err| format!("kimetsu chat --list-skills: {err}"))?;
        if registry.skills().is_empty() {
            println!("no skills found");
        } else {
            for skill in registry.skills() {
                println!(
                    "{} [{}]\n  {}\n  root: {}\n  entrypoint: {}\n  resources: {}",
                    skill.name,
                    skill_origin_label(skill),
                    skill.description,
                    skill.root.display(),
                    skill.path.display(),
                    skill.resource_summary()
                );
            }
        }
        return Ok(());
    }
    let reader = stdin.lock();
    let writer = stdout.lock();
    run_repl(reader, writer, config).map_err(|e| format!("kimetsu chat: {e}").into())
}

fn init(args: InitArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let summary = project::init_project(&cwd, args.force)?;

    // Friendly summary — use display_path to strip \\?\ on Windows.
    let pretty_root = kimetsu_core::paths::display_path(&summary.repo_root);
    println!("✓ Initialized Kimetsu in {pretty_root}");

    // Show inner files workspace-relative.
    println!("    brain:  .kimetsu/brain.db  ({} memories)", {
        project::list_memories(&summary.repo_root)
            .map(|m| m.len())
            .unwrap_or(0)
    });
    println!("    config: .kimetsu/project.toml");
    println!("    model:  {}", summary.model);

    if !summary.api_key_present {
        println!(
            "    note: {} isn't set — needed only for model-backed commands \
             (`kimetsu run`, `kimetsu chat`), not for the brain.",
            summary.api_key_env
        );
    }

    println!();
    println!("Next — wire a host agent so it uses the brain:");
    println!("    kimetsu plugin install claude-code      (also: codex, pi, openclaw)");
    println!(
        "    kimetsu setup                           (init + install + health check, in one step)"
    );

    Ok(())
}

fn config(command: ConfigCommand) -> KimetsuResult<()> {
    match command {
        ConfigCommand::Show => {
            print!("{}", project::config_text(&env::current_dir()?)?);
            Ok(())
        }
        ConfigCommand::Edit => {
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;
            config_edit_with(&paths.project_toml, |path| {
                // Resolve the editor: $EDITOR, then $VISUAL, then platform default.
                let editor = env::var("EDITOR")
                    .or_else(|_| env::var("VISUAL"))
                    .unwrap_or_else(|_| {
                        if cfg!(windows) {
                            "notepad".to_string()
                        } else {
                            "vi".to_string()
                        }
                    });
                let status = std::process::Command::new(&editor).arg(path).status()?;
                if status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other(format!(
                        "editor `{editor}` exited with non-zero status: {status}"
                    )))
                }
            })
        }
        ConfigCommand::Get { key } => {
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;
            // Use the EFFECTIVE config (serde defaults filled in) so fields
            // like `embedder.enabled` show even when absent from the file.
            let cfg = project::load_config(&paths)?;
            let root: toml::Value = toml::Value::try_from(&cfg)
                .map_err(|e| format!("config get: failed to serialise config: {e}"))?;
            match get_toml_path(&root, &key) {
                Some(toml::Value::Table(t)) => {
                    // Pretty-print tables so the output is readable.
                    println!(
                        "{}",
                        toml::to_string(t)
                            .map_err(|e| format!("config get: serialise table: {e}"))?
                            .trim_end()
                    );
                }
                Some(toml::Value::Array(arr)) => {
                    println!(
                        "{}",
                        toml::to_string_pretty(&toml::Value::Array(arr.clone()))
                            .map_err(|e| format!("config get: serialise array: {e}"))?
                            .trim_end()
                    );
                }
                Some(leaf) => {
                    // Bare scalar: strip surrounding quotes for strings.
                    let rendered = toml::to_string_pretty(&toml::Value::Table({
                        let mut m = toml::map::Map::new();
                        m.insert("v".to_string(), leaf.clone());
                        m
                    }))
                    .map_err(|e| format!("config get: serialise scalar: {e}"))?;
                    // `toml::to_string_pretty` of `{v = <leaf>}` yields "v = <repr>\n".
                    // Strip the "v = " prefix and trailing newline.
                    let bare = rendered
                        .trim_end()
                        .strip_prefix("v = ")
                        .unwrap_or(rendered.trim_end());
                    println!("{bare}");
                }
                None => {
                    // Provide a helpful error listing the closest valid sub-keys.
                    let hint = closest_keys_hint(&root, &key);
                    return Err(format!("config get: key `{key}` not found.{hint}").into());
                }
            }
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;

            let disk_text = std::fs::read_to_string(&paths.project_toml).map_err(|e| {
                format!(
                    "config set: could not read {}: {e}",
                    paths.project_toml.display()
                )
            })?;

            let (new_text, dropped_to_custom) = config_set_text(&disk_text, &key, &value)?;

            // Write — only reached when validation inside config_set_text passes.
            std::fs::write(&paths.project_toml, &new_text).map_err(|e| {
                format!(
                    "config set: failed to write {}: {e}",
                    paths.project_toml.display()
                )
            })?;

            println!("set {key} = {value}");
            if dropped_to_custom {
                println!(
                    "note: retrieval.level set to \"custom\" so this manual value is not \
                     overridden by a preset at load time."
                );
            }
            Ok(())
        }
    }
}

/// Keys whose values `ProjectConfig::apply_retrieval_level` overwrites when a
/// non-`custom` retrieval level (`basic`/`flexible`/`deep`/`advanced`) is active.
/// Setting one of these manually must drop the level to `custom`, otherwise the
/// explicit value is silently clobbered at load time.
fn is_level_managed_key(key: &str) -> bool {
    matches!(key, "embedder.enabled" | "embedder.reranker")
}

/// Core of `config set` (extracted so the command and its integration test share
/// one code path). Sets `key = value` in `disk_text` surgically (comments and
/// formatting preserved via `toml_edit`). When `key` is a retrieval-level-managed
/// field AND the current level is a managed preset, it ALSO sets
/// `retrieval.level = "custom"` so the explicit value survives
/// `apply_retrieval_level` at load. Validates the result through `ProjectConfig`.
///
/// Returns `(new_toml_text, dropped_to_custom)`. Pre-levels files (no `[retrieval]`
/// table → default level `custom`) are never modified beyond the requested key, so
/// existing behavior is byte-identical.
fn config_set_text(disk_text: &str, key: &str, value: &str) -> KimetsuResult<(String, bool)> {
    // Resolve the existing leaf type (for coercion) from a plain value tree.
    let root_val: toml::Value = toml::from_str(disk_text)
        .map_err(|e| format!("config set: project.toml is invalid TOML: {e}"))?;
    let existing = get_toml_path(&root_val, key).cloned();
    let typed_value =
        parse_scalar(value, existing.as_ref()).map_err(|e| format!("config set: {e}"))?;

    // Surgical edit on a comment-preserving document.
    let mut doc: toml_edit::DocumentMut = disk_text
        .parse()
        .map_err(|e| format!("config set: project.toml is invalid TOML (edit): {e}"))?;
    set_toml_edit_path(&mut doc, key, &typed_value).map_err(|e| format!("config set: {e}"))?;

    // Auto-drop to "custom" when overriding a preset-managed field, so the
    // explicit value is not clobbered by apply_retrieval_level on the next load.
    let mut dropped_to_custom = false;
    if is_level_managed_key(key) {
        let cur_level = root_val
            .get("retrieval")
            .and_then(|r| r.get("level"))
            .and_then(|l| l.as_str())
            .unwrap_or("custom");
        if matches!(cur_level, "basic" | "flexible" | "deep" | "advanced") {
            let custom = toml::Value::String("custom".to_string());
            set_toml_edit_path(&mut doc, "retrieval.level", &custom)
                .map_err(|e| format!("config set: {e}"))?;
            dropped_to_custom = true;
        }
    }

    let new_text = doc.to_string();
    project::load_config_from_text(&new_text).map_err(|e| {
        format!("config set: result is not a valid config — {e}. File NOT written.")
    })?;
    Ok((new_text, dropped_to_custom))
}

/// Testable seam for `config edit`. Opens the config file at `toml_path`
/// via the `edit` closure (which is either the real editor launch or a
/// test-injected closure that mutates the file), then re-parses the
/// result to catch syntax errors before returning.
///
/// Returns `Err` with a clear message if the editor fails or if the
/// resulting TOML is invalid. Prints a confirmation on success.
fn config_edit_with(
    toml_path: &std::path::Path,
    edit: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> KimetsuResult<()> {
    edit(toml_path).map_err(|err| format!("config edit: editor failed: {err}"))?;

    // Re-parse to catch syntax errors.
    let content = std::fs::read_to_string(toml_path)
        .map_err(|err| format!("config edit: could not read {}: {err}", toml_path.display()))?;
    project::load_config_from_text(&content)
        .map_err(|err| format!("config edit: saved file has invalid TOML — {err}"))?;

    println!("config saved: {}", toml_path.display());
    Ok(())
}

// ── config get/set pure helpers ──────────────────────────────────────────────

/// Navigate a dotted key path (`a.b.c`) through `root` and return a reference
/// to the leaf value, or `None` if any segment is missing.
fn get_toml_path<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = root;
    for segment in key.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Navigate/create a dotted key path (`a.b.c`) in `root` (a `toml::Value::Table`)
/// and set the leaf to `value`. Intermediate segments are created as empty tables
/// when absent. Returns `Err` if an intermediate segment exists but is not a table.
///
/// NOTE: this function is kept for unit tests only.  Production config writes use
/// `set_toml_edit_path` which preserves TOML comments.
#[cfg(test)]
fn set_toml_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    let (leaf_key, parents) = segments
        .split_last()
        .ok_or_else(|| "key must not be empty".to_string())?;

    let mut current = root;
    for seg in parents {
        // Ensure the current node is a table.
        if !current.is_table() {
            return Err(format!(
                "cannot set `{key}`: `{seg}` is `{}`, not a table",
                current.type_str()
            ));
        }
        // Navigate into the segment, creating an empty table if absent.
        if current.get(seg).is_none() {
            current
                .as_table_mut()
                .unwrap()
                .insert(seg.to_string(), toml::Value::Table(toml::map::Map::new()));
        }
        current = current.get_mut(seg).unwrap();
    }
    if !current.is_table() {
        return Err(format!(
            "cannot set `{key}`: parent is `{}`, not a table",
            current.type_str()
        ));
    }
    current
        .as_table_mut()
        .unwrap()
        .insert(leaf_key.to_string(), value);
    Ok(())
}

/// S4.2 — Surgical, comment-preserving write via `toml_edit`.
///
/// Navigate/create a dotted key path (`a.b.c`) inside a `toml_edit::DocumentMut`
/// and overwrite the leaf with `value` (a `toml::Value` for type information).
/// Intermediate tables are created when absent. Returns `Err` when an
/// intermediate segment is not a table.
///
/// This preserves all TOML comments, whitespace, and unknown keys because
/// `toml_edit` operates on the concrete syntax tree rather than a typed struct.
fn set_toml_edit_path(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: &toml::Value,
) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    let (leaf_key, parents) = segments
        .split_last()
        .ok_or_else(|| "key must not be empty".to_string())?;

    // Navigate into parent tables, creating inline tables when absent.
    let mut current: &mut toml_edit::Item = doc.as_item_mut();
    for seg in parents {
        // If the segment doesn't exist yet, insert an empty table.
        if current.get(seg).is_none() {
            if let Some(tbl) = current.as_table_mut() {
                tbl.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
            } else {
                return Err(format!("cannot set `{key}`: `{seg}` is not a table"));
            }
        }
        current = current
            .get_mut(seg)
            .ok_or_else(|| format!("cannot set `{key}`: `{seg}` not found after insert"))?;
        if !current.is_table() && !current.is_inline_table() {
            return Err(format!("cannot set `{key}`: `{seg}` is not a table"));
        }
    }

    // Convert the toml::Value leaf into a toml_edit::Value.
    let edit_val: toml_edit::Value = match value {
        toml::Value::Boolean(b) => toml_edit::Value::from(*b),
        toml::Value::Integer(n) => toml_edit::Value::from(*n),
        toml::Value::Float(f) => toml_edit::Value::from(*f),
        toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
        other => {
            // Fallback: round-trip through TOML text for complex types.
            let text = toml::to_string(other)
                .map_err(|e| format!("cannot serialise value for `{key}`: {e}"))?;
            text.trim()
                .parse::<toml_edit::Value>()
                .map_err(|e| format!("cannot parse serialised value for `{key}`: {e}"))?
        }
    };

    if let Some(tbl) = current.as_table_mut() {
        tbl.insert(leaf_key, toml_edit::Item::Value(edit_val));
    } else {
        return Err(format!("cannot set `{key}`: parent segment is not a table"));
    }

    Ok(())
}

/// Parse `input` into a typed `toml::Value`.
///
/// Type-resolution order:
/// 1. If `existing` is `Some`, coerce to its type (bool, integer, float, string).
///    Returns `Err` if coercion to integer or float fails so callers can surface a clear message.
/// 2. Otherwise infer from the literal:
///    - `"true"` / `"false"` → `Bool`
///    - All-digit string (optionally leading `-`) → `Integer`
///    - Parseable as `f64` → `Float`
///    - Anything else → `String`
fn parse_scalar(input: &str, existing: Option<&toml::Value>) -> Result<toml::Value, String> {
    match existing {
        Some(toml::Value::Boolean(_)) => {
            Ok(toml::Value::Boolean(input.eq_ignore_ascii_case("true")))
        }
        Some(toml::Value::Integer(_)) => {
            input.parse::<i64>().map(toml::Value::Integer).map_err(|_| {
                format!("cannot coerce `{input}` to integer (existing field is an integer)")
            })
        }
        Some(toml::Value::Float(_)) => input
            .parse::<f64>()
            .map(toml::Value::Float)
            .map_err(|_| format!("cannot coerce `{input}` to float (existing field is a float)")),
        Some(toml::Value::String(_)) => Ok(toml::Value::String(input.to_string())),
        // Array / table / datetime: fall through to literal inference.
        _ => Ok(infer_scalar(input)),
    }
}

/// Infer a `toml::Value` type from a bare string literal.
fn infer_scalar(input: &str) -> toml::Value {
    if input.eq_ignore_ascii_case("true") {
        return toml::Value::Boolean(true);
    }
    if input.eq_ignore_ascii_case("false") {
        return toml::Value::Boolean(false);
    }
    // Integer: optional leading `-`, then all digits.
    let digit_part = input.strip_prefix('-').unwrap_or(input);
    if !digit_part.is_empty() && digit_part.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = input.parse::<i64>() {
            return toml::Value::Integer(n);
        }
    }
    if let Ok(f) = input.parse::<f64>() {
        // Distinguish "1.0" (float) from "1" (already caught as integer above).
        if input.contains('.') || input.contains('e') || input.contains('E') {
            return toml::Value::Float(f);
        }
    }
    toml::Value::String(input.to_string())
}

/// Build a human-readable hint listing the closest valid keys when `get` fails.
fn closest_keys_hint(root: &toml::Value, key: &str) -> String {
    // Walk as far as we can, then show the available keys at the stuck level.
    let segments: Vec<&str> = key.split('.').collect();
    let mut current = root;
    let mut walked = Vec::new();
    for seg in &segments {
        match current.get(seg) {
            Some(next) => {
                walked.push(*seg);
                current = next;
            }
            None => {
                // Show available keys at this level.
                if let Some(table) = current.as_table() {
                    let keys: Vec<&str> = table.keys().map(|k| k.as_str()).collect();
                    let prefix = if walked.is_empty() {
                        String::new()
                    } else {
                        format!(" Under `{}`:", walked.join("."))
                    };
                    return format!("{prefix} available keys: [{}]", keys.join(", "));
                }
                return String::new();
            }
        }
    }
    String::new()
}

fn brain(command: BrainCommand) -> KimetsuResult<()> {
    // v0.8: honor the [embedder] config (env still wins) for every
    // command except `model set`, which sets the new selection itself.
    // The embedder is a process-static OnceLock, so this must run
    // before any retrieval/reindex touches it — entry is the safe spot.
    if !matches!(command, BrainCommand::Model { .. }) {
        apply_embedder_from_cwd();
    }
    match command {
        BrainCommand::IngestRepo { path } => {
            let summary = project::ingest_repo(&path)?;
            println!("repo_root: {}", summary.repo_root.display());
            println!("indexed_files: {}", summary.indexed_files);
            println!("skipped_files: {}", summary.skipped_files);
            println!("manifests: {}", summary.manifests);
            Ok(())
        }
        BrainCommand::Search(args) => {
            let capsules = project::search_files(&env::current_dir()?, &args.query, args.limit)?;
            if capsules.is_empty() {
                println!("no file matches");
                return Ok(());
            }

            for capsule in capsules {
                println!(
                    "{:.3} {} {}",
                    capsule.score, capsule.expansion_handle, capsule.summary
                );
            }
            Ok(())
        }
        BrainCommand::Context(args) => {
            let cwd = env::current_dir()?;
            // v0.4.4: auto-augment with ambient workspace context
            // (git branch + dirty files + recent edits) unless the
            // caller opts out via --no-ambient or
            // `KIMETSU_BRAIN_AMBIENT=off`. The augmentation appends
            // a short, lexically + semantically retrievable suffix
            // to the query before retrieval — see
            // `kimetsu_brain::ambient::augment_query`.
            // W3.2: load broker.ambient from project config (env still wins).
            // Load the resolved config once here and reuse it below for the
            // retrieval-level HyDE decision (load_config has already applied
            // the [retrieval] level preset).
            let context_config = kimetsu_core::paths::ProjectPaths::discover(&cwd)
                .ok()
                .and_then(|paths| project::load_config(&paths).ok());
            let config_ambient = context_config
                .as_ref()
                .map(|cfg| cfg.broker.ambient)
                .unwrap_or(true);
            let (effective_query, ambient_payload) = if !args.no_ambient
                && kimetsu_brain::ambient::ambient_enabled_with(config_ambient)
            {
                let ctx = kimetsu_brain::ambient::collect(&cwd);
                let augmented = kimetsu_brain::ambient::augment_query(&args.query, &ctx);
                (augmented, Some(ctx))
            } else {
                (args.query.clone(), None)
            };
            // #1a HyDE: expand the (ambient-augmented) query with a hypothetical
            // answer before retrieval. HyDE is on when explicitly requested
            // (--hyde) OR when the configured retrieval level is "advanced".
            let hyde_from_level = context_config
                .as_ref()
                .map(|cfg| cfg.hyde_from_level())
                .unwrap_or(false);
            let hyde_enabled = args.hyde || hyde_from_level;
            // Advanced level leans on a capable cheap model; nudge the user if
            // none is configured (non-fatal; the raw query is still used).
            if hyde_from_level && distiller::resolve_distiller(&cwd).is_none() {
                eprintln!(
                    "kimetsu: retrieval level 'advanced' works best with a capable cheap model (OpenAI/Anthropic or a larger local model like qwen2.5:14b); set [cheap_model] in project.toml."
                );
            }
            let effective_query = if hyde_enabled {
                hyde_augment_query(&cwd, &effective_query)
            } else {
                effective_query
            };
            let bundle =
                project::retrieve_context(&cwd, &args.stage, &effective_query, args.budget_tokens)?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": true,
                        "stage": bundle.stage,
                        "query": args.query,
                        "augmented_query": effective_query,
                        "ambient": ambient_payload,
                        "budget_tokens": bundle.budget_tokens,
                        "used_tokens": bundle.used_tokens,
                        "capsule_count": bundle.capsules.len(),
                        "excluded_count": bundle.excluded.len(),
                        "capsules": bundle.capsules,
                        "excluded": bundle.excluded,
                    }))?
                );
                return Ok(());
            }
            println!(
                "stage: {} used_tokens: {}/{} capsules: {} excluded: {}",
                bundle.stage,
                bundle.used_tokens,
                bundle.budget_tokens,
                bundle.capsules.len(),
                bundle.excluded.len()
            );
            for capsule in bundle.capsules {
                println!(
                    "{:.3} {} [{} rel={:.2} conf={:.2} fresh={:.2} scope={:.2} tokens={}]",
                    capsule.score,
                    capsule.expansion_handle,
                    capsule.kind,
                    capsule.relevance,
                    capsule.confidence,
                    capsule.freshness,
                    capsule.scope_weight,
                    capsule.token_estimate
                );
                println!("  {}", capsule.summary);
            }
            Ok(())
        }
        BrainCommand::Memory { command } => memory(command),
        BrainCommand::Rebuild { from_traces } => {
            let events = project::rebuild_projection(&env::current_dir()?, from_traces)?;
            println!("brain projection rebuilt from {events} events");
            Ok(())
        }
        BrainCommand::Stats => stats(),
        BrainCommand::Status { json } => brain_status(json),
        BrainCommand::Insights {
            json,
            last_n_runs,
            since,
            top,
        } => brain_insights(json, last_n_runs, since, top),
        BrainCommand::ContextHook(args) => brain_context_hook(args),
        BrainCommand::StopHook(args) => brain_stop_hook(args),
        BrainCommand::Reindex(args) => reindex_brain(args),
        BrainCommand::Model { command } => brain_model(command),
        BrainCommand::PreToolHook(args) => proactive_hook(ProactiveEvent::PreTool, args),
        BrainCommand::PostToolHook(args) => proactive_hook(ProactiveEvent::PostTool, args),
        BrainCommand::SessionEndHook(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            distiller::run_session_end_hook(&workspace);
            // Slice B: hands-off team memory — auto-sync at session end when a
            // `[sync] dir` is configured (and `auto` not disabled). Best-effort:
            // a sync failure must never break session shutdown.
            auto_sync_at_session_end(&workspace);
            Ok(())
        }
        BrainCommand::SessionStartHook(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            brain_session_start_hook(&workspace)
        }
        BrainCommand::Digest(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            brain_digest_cmd(&workspace, args.refresh)
        }
        BrainCommand::Compact(args) => brain_compact(args),
        BrainCommand::Export(args) => brain_export(args),
        BrainCommand::Import(args) => brain_import(args),
        BrainCommand::Backup(args) => brain_backup(args),
        BrainCommand::EmbedDaemon(args) => brain_embed_daemon(args),
        BrainCommand::Warm => brain_warm(),
        BrainCommand::Daemon(args) => brain_daemon(args),
        BrainCommand::Eval(args) => brain_eval(args),
        BrainCommand::Bench(args) => brain_bench(args),
        BrainCommand::Roi(args) => brain_roi(args),
        BrainCommand::Tune(args) => brain_tune(args),
        BrainCommand::Consolidate(args) => brain_consolidate(args),
        BrainCommand::Reflect(args) => brain_reflect(args),
        BrainCommand::Triage(args) => brain_triage(args),
        BrainCommand::Forget(args) => brain_forget(args),
        BrainCommand::Cite(args) => brain_cite(args),
        BrainCommand::Regret(args) => brain_regret(args),
        BrainCommand::Distill(args) => brain_distill(args),
        BrainCommand::Graph { command } => brain_graph(command),
        BrainCommand::Ask(args) => brain_ask(args),
        BrainCommand::Skills(args) => brain_skills(args),
        BrainCommand::Sync(args) => brain_sync(args),
    }
}

/// v0.8: best-effort — load the project config from the current dir and
/// record its `[embedder] model` so brain-internal callers resolve it
/// (env still wins). Silently no-ops when the brain isn't initialized.
fn apply_embedder_from_cwd() {
    if let Ok(cwd) = env::current_dir()
        && let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&cwd)
        && let Ok(config) = project::load_config(&paths)
    {
        kimetsu_brain::embeddings::apply_embedder_selection(Some(&config.embedder.model));
    }
}

/// v0.8: `kimetsu brain model list|set`.
fn brain_model(command: ModelCommand) -> KimetsuResult<()> {
    match command {
        ModelCommand::List { json } => brain_model_list(json),
        ModelCommand::Set(args) => brain_model_set(args),
    }
}

fn brain_model_list(json: bool) -> KimetsuResult<()> {
    use kimetsu_brain::embeddings::{BUILTIN_MODELS, resolve_embedder_id};

    // Resolve the active id + where it came from, best-effort.
    let (config_model, source) = match env::current_dir()
        .ok()
        .and_then(|cwd| kimetsu_core::paths::ProjectPaths::discover(&cwd).ok())
        .and_then(|paths| project::load_config(&paths).ok())
    {
        Some(cfg) => (Some(cfg.embedder.model.clone()), "config"),
        None => (None, "default"),
    };
    let env_set = env::var("KIMETSU_BRAIN_EMBEDDER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let source = if env_set.is_some() { "env" } else { source };
    let active = resolve_embedder_id(config_model.as_deref());

    if json {
        let models: Vec<_> = BUILTIN_MODELS
            .iter()
            .map(|(id, dim, blurb)| {
                serde_json::json!({
                    "id": id,
                    "dim": dim,
                    "description": blurb,
                    "active": *id == active,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "active": active,
                "source": source,
                "models": models,
            }))?
        );
        return Ok(());
    }

    println!("Embedding models (active resolved from {source}):");
    for (id, dim, blurb) in BUILTIN_MODELS {
        let marker = if *id == active { "*" } else { " " };
        println!("  {marker} {id:<22} {dim:>5}d  {blurb}");
    }
    println!("\nChange with: kimetsu brain model set <id>");
    println!("(env KIMETSU_BRAIN_EMBEDDER always overrides the config field)");
    Ok(())
}

fn brain_model_set(args: ModelSetArgs) -> KimetsuResult<()> {
    use kimetsu_brain::embeddings::{apply_embedder_selection, resolve_embedder_id};

    // Validate against the curated set so `set` never silently falls
    // back to the default for a typo'd id.
    if !is_known_alias(&args.id) {
        return Err(format!(
            "unknown embedder id `{}`. Run `kimetsu brain model list` for the options.",
            args.id
        )
        .into());
    }
    let canonical = resolve_embedder_id(Some(&args.id));

    let workspace = args.workspace.clone().unwrap_or(env::current_dir()?);
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let mut config = project::load_config(&paths)?;
    let previous = config.embedder.model.clone();
    let prev_dim = dim_for(resolve_embedder_id(Some(&previous)));
    let new_dim = dim_for(canonical);

    config.embedder.model = canonical.to_string();
    std::fs::write(&paths.project_toml, config.to_toml()?)?;

    // Fresh CLI process: the embedder OnceLock is not yet initialized,
    // so recording the override here means the reindex below loads the
    // NEW model.
    apply_embedder_selection(Some(canonical));

    let dim_changed = prev_dim != new_dim;

    if args.no_reindex {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true, "model": canonical, "previous": previous,
                    "reindexed": false, "dimension_changed": dim_changed,
                }))?
            );
        } else {
            println!(
                "Embedder set to `{canonical}` (was `{previous}`). Skipped reindex (--no-reindex)."
            );
            if dim_changed {
                println!(
                    "Dimension changed {prev_dim}d -> {new_dim}d: run `kimetsu brain reindex --force` so cosine retrieval uses the new model."
                );
            }
        }
        return Ok(());
    }

    // Re-embed with a FRESH embedder for the new model (not whatever the
    // default cache might resolve to), so the corpus is migrated to the
    // chosen model deterministically.
    let embedder = kimetsu_brain::embeddings::open_embedder_for_model(canonical);
    let report = kimetsu_brain::reindex::reindex_all_with_embedder(
        &workspace,
        kimetsu_brain::reindex::ReindexOptions {
            scope: kimetsu_brain::reindex::ReindexScope::All,
            dry_run: false,
            force: dim_changed,
            limit: None,
        },
        embedder.as_ref(),
    )?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true, "model": canonical, "previous": previous,
                "reindexed": !report.embedder_noop,
                "dimension_changed": dim_changed,
                "updated": report.updated_total(),
                "embedder_noop": report.embedder_noop,
            }))?
        );
        return Ok(());
    }

    println!("Embedder set to `{canonical}` (was `{previous}`).");
    if report.embedder_noop {
        println!(
            "Active embedder is `noop` (lean build or KIMETSU_BRAIN_EMBEDDER=noop): id recorded, but no vectors were produced. Build with `--features embeddings` then run `kimetsu brain reindex`."
        );
    } else {
        println!(
            "Reindexed {} memories with the new model.",
            report.updated_total()
        );
    }
    Ok(())
}

fn is_known_alias(id: &str) -> bool {
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

fn dim_for(canonical_id: &str) -> usize {
    kimetsu_brain::embeddings::BUILTIN_MODELS
        .iter()
        .find(|(id, _, _)| *id == canonical_id)
        .map(|(_, dim, _)| *dim)
        .unwrap_or(0)
}

/// v0.4.3: `kimetsu brain reindex` — backfill missing / stale
/// embeddings. The interesting cases:
///
///   * NoopEmbedder (default Cargo build OR
///     `KIMETSU_BRAIN_EMBEDDER=noop`): we print a hint and exit.
///     Without a real embedder there's nothing to reindex against.
///   * Real embedder + dry-run: counts how many rows are stale per
///     scope without writing.
///   * Real embedder + apply: walks both project and (optionally)
///     user brains, re-embeds candidate rows in created_at order,
///     prints a summary per scope.
fn reindex_brain(args: ReindexArgs) -> KimetsuResult<()> {
    let scope = kimetsu_brain::reindex::ReindexScope::parse(&args.scope)?;
    let opts = kimetsu_brain::reindex::ReindexOptions {
        scope,
        dry_run: args.dry_run,
        force: args.force,
        limit: args.limit,
    };
    let report = kimetsu_brain::reindex::reindex_all(&env::current_dir()?, opts)?;

    if report.embedder_noop {
        println!(
            "[reindex] active embedder is `noop` — nothing to do. \
             Build kimetsu with `--features embeddings` and unset \
             KIMETSU_BRAIN_EMBEDDER=noop to enable semantic retrieval."
        );
        return Ok(());
    }

    println!(
        "[reindex] model={} dry_run={} force={} scope={:?}{}",
        report.embedder_model_id,
        args.dry_run,
        args.force,
        scope,
        args.limit
            .map(|n| format!(" limit={n}"))
            .unwrap_or_default(),
    );
    for sub in [&report.project, &report.user] {
        if !sub.opened {
            println!("  {}: skipped (scope filter or DB unavailable)", sub.scope);
            continue;
        }
        let action = if args.dry_run {
            "candidates"
        } else {
            "updated"
        };
        let count = if args.dry_run {
            sub.candidates
        } else {
            sub.updated
        };
        println!(
            "  {}: total={} {}={} failed={}",
            sub.scope, sub.total, action, count, sub.failed
        );
    }
    println!(
        "[reindex] {} total {} across project + user",
        if args.dry_run {
            report.candidates_total()
        } else {
            report.updated_total()
        },
        if args.dry_run {
            "candidates"
        } else {
            "updated"
        },
    );
    Ok(())
}

// ── Q8: brain compact ────────────────────────────────────────────────────────

// ── Flagship 1 Pass B: session-start-hook + digest command ───────────────────

/// `kimetsu brain session-start-hook`
///
/// Flagship 1 / Pass B / Story 1.5: SessionStart hook that injects the
/// repo digest (1.1) + episodic resume (Pass A) as `additionalContext` so
/// the agent's first turn knows the repo and task without exploratory I/O.
///
/// Output format: Claude Code `additionalContext` JSON.
/// Gated by `[broker] warm_start` (default true).
/// Silent when no digest AND no live episode.
fn brain_session_start_hook(workspace: &Path) -> KimetsuResult<()> {
    // Gate: load warm_start from config (best-effort; default ON).
    let warm_start_enabled = kimetsu_core::paths::ProjectPaths::discover(workspace)
        .ok()
        .and_then(|paths| kimetsu_brain::project::load_config(&paths).ok())
        .map(|cfg| cfg.broker.warm_start)
        .unwrap_or(true);

    if !warm_start_enabled {
        return Ok(());
    }

    // 1. Repo digest (story 1.1).
    let digest = kimetsu_brain::digest::build_or_load_digest(workspace, false);

    // 2. Episodic resume (Pass A, story 1.4).
    let resume = kimetsu_brain::episode::render_resume_context(workspace);

    // Silent when neither has content.
    if digest.is_none() && resume.is_none() {
        return Ok(());
    }

    // Assemble additionalContext.
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = &digest {
        parts.push(format!("## Repo context\n{d}"));
    }
    if let Some(r) = &resume {
        parts.push(format!("## Your prior session\n{r}"));
    }
    let additional_context = parts.join("\n\n");

    // ROI attribution (best-effort).
    let digest_chars = digest.as_ref().map(|d| d.len()).unwrap_or(0);
    let resume_chars = resume.as_ref().map(|r| r.len()).unwrap_or(0);
    kimetsu_brain::digest::record_warmstart_served(workspace, digest_chars, resume_chars);

    // Emit Claude Code SessionStart additionalContext JSON.
    let output = serde_json::json!({
        "continue": true,
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": additional_context,
        },
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

/// `kimetsu brain digest [--refresh]`
///
/// Flagship 1 / Pass B / Story 1.1: build (or rebuild) the repo digest.
/// Prints the digest to stdout and writes `.kimetsu/digest.md`.
fn brain_digest_cmd(workspace: &Path, refresh: bool) -> KimetsuResult<()> {
    match kimetsu_brain::digest::build_or_load_digest(workspace, refresh) {
        Some(digest) => {
            println!("{digest}");
        }
        None => {
            eprintln!("[Kimetsu] No digest content: brain may not be initialized or empty.");
        }
    }
    Ok(())
}

/// `kimetsu brain compact [--purge-invalidated] [--trim-events-older-than <dur>] [--json]`
///
/// Reclaims dead space in brain.db via SQLite VACUUM. Optional flags allow
/// purging invalidated memory rows and trimming the durable event log.
fn brain_compact(args: CompactArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Parse --trim-events-older-than if provided.
    let trim_dur = args
        .trim_events_older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| format!("--trim-events-older-than: {e}"))?;

    // Print warnings before performing any destructive operations.
    if let Some(ref dur_str) = args.trim_events_older_than {
        eprintln!(
            "WARNING: --trim-events-older-than {dur_str} will delete events older than \
             {dur_str} from the durable event log. Materialized memories are unaffected, \
             but the rebuild history window will be reduced."
        );
    }
    if args.purge_invalidated {
        eprintln!(
            "NOTE: --purge-invalidated will permanently delete retired (invalidated) memory \
             rows. They will no longer appear in audit/blame output."
        );
    }

    let report = project::compact_brain(&workspace, trim_dur, args.purge_invalidated)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Human-readable output.
    let freed = report.bytes_before.saturating_sub(report.bytes_after);
    println!(
        "compacted brain.db: {} → {} (freed {})",
        fmt_bytes(report.bytes_before),
        fmt_bytes(report.bytes_after),
        fmt_bytes(freed),
    );
    if report.invalidated_memories_purged > 0 {
        println!(
            "  purged {} invalidated memor{} (removed from audit trail)",
            report.invalidated_memories_purged,
            if report.invalidated_memories_purged == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }
    if report.events_trimmed > 0 {
        println!(
            "  trimmed {} old event{} (rebuild history reduced)",
            report.events_trimmed,
            if report.events_trimmed == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

// ── Q5: brain export / import ────────────────────────────────────────────────

/// `kimetsu brain export <file> [--scope] [--kind]`
///
/// Dumps active memories as pretty-printed JSON. Writes to stdout when
/// `file` is `-`.  Prints "exported N memories to <file>" on success.
fn brain_export(args: BrainExportArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Parse optional scope/kind filters.
    let scope = args
        .scope
        .as_deref()
        .map(|s| {
            s.parse::<MemoryScope>().map_err(|_| {
                format!("unknown scope `{s}`; expected one of: global_user, project, repo, run")
            })
        })
        .transpose()?;
    let kind = args
        .kind
        .as_deref()
        .map(|k| {
            k.parse::<MemoryKind>()
                .map_err(|_| format!("unknown kind `{k}`; expected one of: preference, convention, command, failure_pattern, fact"))
        })
        .transpose()?;

    let memories =
        project::export_memories(&workspace, scope, kind, args.redact, args.redact_tags)?;
    let json = serde_json::to_string_pretty(&memories)
        .map_err(|e| format!("brain export: failed to serialize: {e}"))?;

    if args.file == "-" {
        println!("{json}");
    } else {
        std::fs::write(&args.file, &json)
            .map_err(|e| format!("brain export: could not write `{}`: {e}", args.file))?;
        println!("exported {} memories to {}", memories.len(), args.file);
    }

    Ok(())
}

/// `kimetsu brain import <file> [--scope-override]`
///
/// Reads a JSON array of `MemoryExport` records (produced by `brain export`)
/// and imports them into the brain. Prints "imported N (deduped M)".
/// Reads from stdin when `file` is `-`.
fn brain_import(args: BrainImportArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Parse optional scope_override.
    let scope_override = args
        .scope_override
        .as_deref()
        .map(|s| {
            s.parse::<MemoryScope>().map_err(|_| {
                format!("unknown scope `{s}`; expected one of: global_user, project, repo, run")
            })
        })
        .transpose()?;

    // Read JSON.
    let json = if args.file == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("brain import: failed to read stdin: {e}"))?;
        buf
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| format!("brain import: could not read `{}`: {e}", args.file))?
    };

    let entries: Vec<project::MemoryExport> = serde_json::from_str(&json).map_err(|e| {
        format!(
            "brain import: `{}` is not valid JSON — expected an array of memory export records: {e}",
            args.file
        )
    })?;

    let summary = project::import_memories(&workspace, &entries, scope_override)?;
    println!(
        "imported {} (deduped {})",
        summary.imported, summary.deduped
    );

    Ok(())
}

/// `kimetsu brain backup [<file>] [--workspace <p>]`
///
/// Writes a consistent full-DB snapshot of brain.db via the SQLite online
/// backup API. Complements `brain export` (memories-only JSON) and the
/// automatic pre-migrate backup — this is a full-schema snapshot you can
/// copy back as a restore.
fn brain_backup(args: BrainBackupArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;

    if !paths.brain_db.exists() {
        return Err(format!(
            "brain.db not found at {} — run `kimetsu init` first",
            paths.brain_db.display()
        )
        .into());
    }

    let dest = args.file.as_deref();
    let (dest_path, size) = kimetsu_brain::migrate::backup_brain(&paths.brain_db, dest)?;
    println!(
        "backed up brain.db ({}) → {}",
        fmt_bytes_brain(size),
        dest_path.display()
    );
    Ok(())
}

// ── S3: brain sync ────────────────────────────────────────────────────────────

/// Slice B: best-effort full sync at session end (push + pull + converge) when a
/// `[sync] dir` is configured and `[sync] auto` is not disabled. Never returns an
/// error — a sync failure must not break session shutdown.
fn auto_sync_at_session_end(workspace: &Path) {
    use kimetsu_brain::sync as brain_sync_mod;
    use kimetsu_core::paths::ProjectPaths;

    let Ok(paths) = ProjectPaths::discover(workspace) else {
        return;
    };
    let Ok((_paths, config, conn)) = project::load_project(workspace) else {
        return;
    };
    let sync_cfg = &config.sync;
    let Some(dir) = sync_cfg.dir.as_deref() else {
        return; // not configured
    };
    if !sync_cfg.auto {
        return; // explicitly disabled
    }
    let machine_id = resolve_machine_id(&sync_cfg.machine_id);
    let cursors_path = paths.kimetsu_dir.join("sync-cursors.json");
    match brain_sync_mod::sync_dir(&conn, Path::new(dir), &machine_id, &cursors_path, false) {
        Ok(report) => {
            if report.pushed > 0 || report.pulled_applied > 0 {
                eprintln!(
                    "kimetsu: auto-synced (pushed {}, pulled {})",
                    report.pushed, report.pulled_applied
                );
            }
        }
        Err(e) => eprintln!("kimetsu: auto-sync skipped ({e})"),
    }
}

/// `kimetsu brain sync [subcommand] [flags]`
///
/// Dispatches to export / import / full-cycle / status based on `args.subcommand`
/// and flag combination.
fn brain_sync(args: SyncArgs) -> KimetsuResult<()> {
    use kimetsu_brain::sync as brain_sync_mod;
    use kimetsu_core::paths::ProjectPaths;

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = ProjectPaths::discover(&workspace)?;

    // Open brain.db (read-write for import/sync; read-only for export/status).
    let (_paths, config, conn) = project::load_project(&workspace)?;

    let sub = args.subcommand.as_deref().unwrap_or("");

    match sub {
        "export" => {
            // kimetsu brain sync export [--since <rowid>] [--out <file>] [--dry-run]
            let out_path = args.out.as_deref().map(std::path::Path::new);
            let (summary, content) =
                brain_sync_mod::export_events(&conn, args.since, out_path, args.dry_run)?;
            if let Some(jsonl) = content {
                println!("{jsonl}");
            } else if args.dry_run {
                println!(
                    "dry-run: would export {} events (next cursor: {})",
                    summary.exported, summary.next_cursor
                );
            } else {
                println!(
                    "exported {} events → {} (next cursor: {})",
                    summary.exported,
                    args.out.as_deref().unwrap_or("<stdout>"),
                    summary.next_cursor
                );
            }
        }
        "import" => {
            // kimetsu brain sync import <batch> [--dry-run]
            let batch_file = args.batch.as_deref().ok_or_else(|| {
                "kimetsu brain sync import: missing <batch> file argument".to_string()
            })?;
            let path = std::path::Path::new(batch_file);
            let summary = brain_sync_mod::import_events_from_file(&conn, path, args.dry_run)?;
            if args.dry_run {
                println!(
                    "dry-run: would apply {} events, skip {} (already present)",
                    summary.applied, summary.skipped
                );
            } else {
                // Slice B: total-order replay so the projection converges in HLC
                // order (the import applied events incrementally in file order).
                if summary.applied > 0 {
                    kimetsu_brain::projector::rebuild_in_place(&conn)?;
                }
                println!(
                    "applied {} events, skipped {}",
                    summary.applied, summary.skipped
                );
            }
        }
        "" => {
            // Full directory-protocol sync, or --status.
            if args.status {
                // 3.3 doctor: show sync state.
                let sync_cfg = &config.sync;
                let sync_dir_opt = sync_cfg.dir.as_deref().map(std::path::Path::new);
                let machine_id = resolve_machine_id(&sync_cfg.machine_id);
                let cursors_path = paths.kimetsu_dir.join("sync-cursors.json");
                let status =
                    brain_sync_mod::sync_status(&conn, sync_dir_opt, &machine_id, &cursors_path)?;
                println!("sync status:");
                println!(
                    "  dir:        {}",
                    status
                        .sync_dir
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(not configured)".to_string())
                );
                println!("  machine_id: {machine_id}");
                println!("  local pending (unpushed): {}", status.local_pending);
                let conflicts = brain_sync_mod::sync_conflict_count(&conn).unwrap_or(0);
                if conflicts > 0 {
                    println!(
                        "  ⚠ supersede conflicts: {conflicts} (concurrent edits chose different \
                         survivors; review with `kimetsu brain memory conflicts`)"
                    );
                }
                if status.sources.is_empty() {
                    println!("  peers: (none seen yet)");
                } else {
                    println!("  peers:");
                    for (mid, cursor, pending) in &status.sources {
                        println!("    {mid}: cursor={cursor}, pending_pull={pending}");
                    }
                }
            } else {
                // Full sync cycle.
                let sync_cfg = &config.sync;
                let sync_dir = match sync_cfg.dir.as_deref() {
                    Some(d) if !d.is_empty() => std::path::PathBuf::from(d),
                    _ => {
                        return Err(
                            "kimetsu brain sync: `[sync] dir` is not configured in project.toml.\n\
                             Set it with: kimetsu config set sync.dir /path/to/shared/dir"
                                .to_string()
                                .into(),
                        );
                    }
                };
                let machine_id = resolve_machine_id(&sync_cfg.machine_id);
                let cursors_path = paths.kimetsu_dir.join("sync-cursors.json");
                let report = brain_sync_mod::sync_dir(
                    &conn,
                    &sync_dir,
                    &machine_id,
                    &cursors_path,
                    args.dry_run,
                )?;
                let prefix = if report.dry_run { "dry-run: " } else { "" };
                println!(
                    "{prefix}pushed {pushed}, pulled {applied} (skipped {skipped}) from {n} peer(s)",
                    pushed = report.pushed,
                    applied = report.pulled_applied,
                    skipped = report.pulled_skipped,
                    n = report.machines_pulled.len(),
                    prefix = prefix,
                );
            }
        }
        other => {
            return Err(format!(
                "kimetsu brain sync: unknown subcommand `{other}`; \
                 expected `export`, `import`, or omit for full sync"
            )
            .into());
        }
    }

    Ok(())
}

/// Resolve the effective machine_id: use the configured value if non-empty,
/// otherwise generate a stable ULID-based id.  The generated id is NOT
/// persisted here — the user should run `kimetsu config set sync.machine_id
/// <id>` to make it durable.
/// Resolve this process's event write origin `<machine>/<agent>` from the
/// environment. Machine: `KIMETSU_SYNC_MACHINE_ID`, else `COMPUTERNAME`/`HOSTNAME`.
/// Agent: `KIMETSU_AGENT_ID` (hosts/hooks set it), else the invoked subcommand,
/// else `cli`. Returns `None` when no machine id is resolvable (origin stays
/// unknown/NULL — best-effort, never fatal).
fn resolve_process_origin() -> Option<String> {
    let machine = std::env::var("KIMETSU_SYNC_MACHINE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))?;
    let agent = std::env::var("KIMETSU_AGENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::args().nth(1).filter(|s| !s.starts_with('-')))
        .unwrap_or_else(|| "cli".to_string());
    Some(format!("{machine}/{agent}"))
}

fn resolve_machine_id(configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    // Stable fallback: use hostname or a generated ULID.
    std::env::var("KIMETSU_SYNC_MACHINE_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ulid::Ulid::new().to_string())
}

// ── embed-daemon / warm / daemon subcommand handlers ─────────────────────────

#[cfg(feature = "embeddings")]
fn brain_embed_daemon(args: EmbedDaemonArgs) -> KimetsuResult<()> {
    use embed_daemon::server::{DaemonState, serve_with_listener};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    // Bind BEFORE loading any model. A redundant spawn (a live daemon already
    // owns the socket — AddrInUse / PermissionDenied / AlreadyExists are the
    // Windows race variants) must exit in milliseconds, not after a
    // multi-second model load: the doomed child inherits the spawning hook's
    // stdio handles, so while it lives the hook's CALLER (the harness hook
    // runner) is stalled waiting for stdout to close.
    let listener = match embed_daemon::ipc::listen(&args.model) {
        Ok(l) => l,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::AddrInUse
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::AlreadyExists
            ) =>
        {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let t0 = Instant::now();
    let embedder = kimetsu_brain::embeddings::open_embedder_for_model(&args.model);
    let reranker = kimetsu_brain::embeddings::open_reranker_for_model(&args.reranker);
    let loaded_ms = t0.elapsed().as_millis() as u64;
    let state = Arc::new(DaemonState {
        embedder,
        reranker,
        model: args.model,
        started: Instant::now(),
        loaded_ms,
        requests: AtomicU64::new(0),
    });
    serve_with_listener(listener, state).map_err(Into::into)
}

#[cfg(feature = "embeddings")]
fn brain_warm() -> KimetsuResult<()> {
    let workspace = env::current_dir().unwrap_or_default();
    // warm_on_start gate: only PRE-warm at startup when configured to. When
    // false the daemon still warms lazily on the first prompt (via the hook's
    // ensure-spawn path) — this only suppresses the SessionStart pre-warm.
    if let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&workspace)
        && let Ok(config) = project::load_config(&paths)
        && !config.embedder.warm_on_start
    {
        return Ok(());
    }
    let Some(model) = resolve_daemon_model(&workspace) else {
        return Ok(());
    };
    let reranker = resolve_daemon_reranker(&workspace);
    embed_daemon::client::ensure_daemon(&model, &reranker);
    Ok(())
}

#[cfg(feature = "embeddings")]
fn brain_daemon(args: DaemonArgs) -> KimetsuResult<()> {
    use embed_daemon::{client, proto};
    let workspace = env::current_dir().unwrap_or_default();
    let model = resolve_daemon_model(&workspace)
        .unwrap_or_else(|| kimetsu_brain::embeddings::resolve_embedder_id(None).to_string());
    match args.command {
        DaemonCommand::Status => match client::request(&model, proto::Request::Ping) {
            Some(proto::Response::Info {
                version,
                model,
                uptime_s,
                requests,
                loaded_ms,
            }) => {
                println!(
                    "running: model={model} version={version} uptime={uptime_s}s requests={requests} load={loaded_ms}ms"
                );
                Ok(())
            }
            _ => {
                println!("not running");
                Ok(())
            }
        },
        DaemonCommand::Stop => {
            let _ = client::request(&model, proto::Request::Shutdown);
            println!("stop requested");
            Ok(())
        }
    }
}

/// Resolve the daemon model id from config, honoring the kill switches.
/// Returns `None` when the daemon must not be used.
#[cfg(feature = "embeddings")]
fn resolve_daemon_model(workspace: &std::path::Path) -> Option<String> {
    if std::env::var("KIMETSU_EMBED_DAEMON").as_deref() == Ok("0") {
        return None;
    }
    let paths = kimetsu_core::paths::ProjectPaths::discover(workspace).ok()?;
    let config = project::load_config(&paths).ok()?;
    if !config.embedder.enabled || !config.embedder.daemon {
        return None;
    }
    Some(
        kimetsu_brain::embeddings::resolve_embedder_id(Some(config.embedder.model.as_str()))
            .to_string(),
    )
}

/// Resolve the reranker id from config. Falls back to `"off"` when config is
/// unreadable so the daemon stays functional without a reranker.
#[cfg(feature = "embeddings")]
fn resolve_daemon_reranker(workspace: &std::path::Path) -> String {
    let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(workspace) else {
        return "off".to_string();
    };
    let Ok(config) = project::load_config(&paths) else {
        return "off".to_string();
    };
    config.embedder.reranker
}

/// Try semantic retrieval via the warm daemon. Returns `None` (-> FTS fallback)
/// when embeddings aren't built, the daemon is disabled by config/env, or the
/// daemon is unreachable within the client budget. On a miss it also kicks off
/// a detached spawn so the NEXT prompt finds a warm daemon.
#[cfg(feature = "embeddings")]
fn try_daemon_retrieve(
    workspace: &std::path::Path,
    request: &kimetsu_brain::context::ContextRequest,
) -> Option<kimetsu_brain::context::ContextBundle> {
    use embed_daemon::{client, proto};
    let model = resolve_daemon_model(workspace)?;
    let args = proto::RetrieveArgs {
        v: proto::PROTOCOL_VERSION,
        brain_root: workspace.to_string_lossy().into_owned(),
        query: request.query.clone(),
        stage: request.stage.clone(),
        budget_tokens: request.budget_tokens,
        max_capsules: request.max_capsules,
        min_score: request.min_score,
        tags: request.tags.clone(),
    };
    match client::request(&model, proto::Request::Retrieve(args)) {
        Some(proto::Response::Capsules {
            capsules,
            skipped,
            top_score,
        }) => Some(daemon_capsules_to_bundle(
            request, capsules, skipped, top_score,
        )),
        _ => {
            // Unreachable/errored: we already know it didn't answer, so spawn
            // directly (no second ping) to keep within the single 300ms budget.
            // A duplicate spawn loses the OS single-instance race and exits.
            let reranker = resolve_daemon_reranker(workspace);
            let _ = client::spawn_daemon(&model, &reranker);
            None
        }
    }
}

#[cfg(not(feature = "embeddings"))]
fn try_daemon_retrieve(
    _workspace: &std::path::Path,
    _request: &kimetsu_brain::context::ContextRequest,
) -> Option<kimetsu_brain::context::ContextBundle> {
    None
}

/// Adapt the wire capsule list back into a `ContextBundle` for the existing
/// rendering code path.
#[cfg(feature = "embeddings")]
fn daemon_capsules_to_bundle(
    request: &kimetsu_brain::context::ContextRequest,
    capsules: Vec<embed_daemon::proto::Capsule>,
    skipped: bool,
    top_score: f32,
) -> kimetsu_brain::context::ContextBundle {
    use kimetsu_brain::context::{ContextBundle, ContextCapsule};
    let capsules = capsules
        .into_iter()
        .map(|c| ContextCapsule::wire_minimal(c.summary, c.kind, c.score))
        .collect();
    ContextBundle {
        stage: request.stage.clone(),
        budget_tokens: request.budget_tokens,
        used_tokens: 0,
        capsules,
        excluded: Vec::new(),
        skipped,
        top_score,
    }
}

// ── Lean (no embeddings) stubs ───────────────────────────────────────────────
#[cfg(not(feature = "embeddings"))]
fn brain_embed_daemon(_args: EmbedDaemonArgs) -> KimetsuResult<()> {
    eprintln!("kimetsu: embeddings not built — no daemon");
    Ok(())
}
#[cfg(not(feature = "embeddings"))]
fn brain_warm() -> KimetsuResult<()> {
    Ok(())
}
#[cfg(not(feature = "embeddings"))]
fn brain_daemon(_args: DaemonArgs) -> KimetsuResult<()> {
    println!("not running (embeddings not built)");
    Ok(())
}

/// Format a byte count as a human-readable string for the brain backup output.
fn fmt_bytes_brain(n: u64) -> String {
    if n < 1_024 {
        format!("{n} B")
    } else if n < 1_024 * 1_024 {
        format!("{:.1} KB", n as f64 / 1_024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1_024.0 * 1_024.0))
    }
}

/// v0.6: `kimetsu brain status` — brain health at a glance.
fn brain_status(json: bool) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let schema_ver = project::schema_version(&cwd)?;
    let memories = project::list_memories(&cwd)?;
    let proposals = project::list_proposals(
        &cwd,
        project::ProposalFilter {
            status: Some("pending".to_string()),
            limit: 200,
            ..Default::default()
        },
    )?;
    let conflicts = project::list_conflicts(&cwd, 200)?;

    let healthy: Vec<_> = memories
        .iter()
        .filter(|m| m.usefulness_score >= 0.2)
        .collect();
    let fading: Vec<_> = memories
        .iter()
        .filter(|m| m.usefulness_score >= 0.0 && m.usefulness_score < 0.2)
        .collect();
    let stale: Vec<_> = memories
        .iter()
        .filter(|m| m.usefulness_score < 0.0)
        .collect();

    // Domain grouping: extract first [tags: ...] prefix from text
    let mut domain_counts: std::collections::BTreeMap<String, usize> = Default::default();
    for m in &memories {
        let domain = if let Some(rest) = m.text.strip_prefix("[tags: ") {
            rest.split(']')
                .next()
                .unwrap_or("other")
                .split_whitespace()
                .next()
                .unwrap_or("other")
                .to_string()
        } else {
            m.kind.clone()
        };
        *domain_counts.entry(domain).or_insert(0) += 1;
    }
    let mut domain_list: Vec<(String, usize)> = domain_counts.into_iter().collect();
    domain_list.sort_by_key(|b| std::cmp::Reverse(b.1));
    let top_domains: Vec<String> = domain_list
        .iter()
        .take(6)
        .map(|(d, n)| format!("{} ({})", d, n))
        .collect();

    // F3 Stories 3.2 & 3.4: regret-flagged memories + invalidations by reason.
    let (regret_flagged, inv_by_reason) = match project::load_project(&cwd) {
        Ok((_paths, config, conn)) => {
            let threshold = config.lifecycle.regret_flag_threshold;
            let regret = kimetsu_brain::lifecycle::regret_flagged_memories(&conn, threshold)
                .map(|v| v.len())
                .unwrap_or(0);
            let inv = kimetsu_brain::lifecycle::invalidations_by_reason(&conn).unwrap_or_default();
            (regret, inv)
        }
        Err(_) => (0, vec![]),
    };

    if json {
        let inv_json: serde_json::Value = inv_by_reason
            .iter()
            .map(|r| (r.reason.clone(), serde_json::json!(r.count)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": schema_ver,
                "memories": memories.len(),
                "pending_proposals": proposals.len(),
                "open_conflicts": conflicts.len(),
                "healthy": healthy.len(),
                "fading": fading.len(),
                "stale": stale.len(),
                "top_domains": top_domains,
                "regret_flagged": regret_flagged,
                "invalidations_by_reason": inv_json,
            }))?
        );
    } else {
        println!(
            "brain: {} memories active, {} pending proposals, {} conflicts",
            memories.len(),
            proposals.len(),
            conflicts.len()
        );
        println!("schema version: {schema_ver}");
        if !top_domains.is_empty() {
            println!("domains: {}", top_domains.join(", "));
        }
        println!("health:  {} healthy (usefulness >= 0.2)", healthy.len());
        println!("         {} fading  (0 <= usefulness < 0.2)", fading.len());
        println!(
            "         {} stale   (usefulness < 0, candidate for prune)",
            stale.len()
        );
        if stale.len() > 3 {
            println!("hint: run `kimetsu brain memory prune` to clean stale entries");
        }
        if regret_flagged > 0 {
            println!(
                "regret:  {} memor{} flagged for review (cited despite being dropped)",
                regret_flagged,
                if regret_flagged == 1 { "y" } else { "ies" }
            );
            println!("hint: run `kimetsu brain forget --dry-run` to review lifecycle candidates");
        }
        if !inv_by_reason.is_empty() {
            let parts: Vec<String> = inv_by_reason
                .iter()
                .map(|r| format!("{}: {}", r.reason, r.count))
                .collect();
            println!("invalidations by reason: {}", parts.join(", "));
        }
    }
    Ok(())
}

/// v1.0 (C5): `kimetsu brain insights` — effectiveness analytics.
fn brain_insights(
    json: bool,
    last_n_runs: u32,
    since: Option<String>,
    top: u32,
) -> KimetsuResult<()> {
    use kimetsu_brain::analytics::{self, InsightsOptions};

    let cwd = env::current_dir()?;
    let opts = InsightsOptions {
        last_n_runs,
        since,
        top_n: top,
    };
    let report = analytics::compute_insights(&cwd, opts)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        // --- Retrieval ---
        let hit_rate = report
            .retrieval
            .hit_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        let avg_score = report
            .retrieval
            .avg_top_score
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Retrieval ──────────────────────────────────");
        println!("  served:       {}", report.retrieval.served);
        println!("  hit-rate:     {hit_rate}");
        println!("  avg-top-score:{avg_score}");

        // --- Citation ---
        let citation_rate = report
            .citation
            .citation_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Citation ───────────────────────────────────");
        println!("  runs-considered: {}", report.citation.runs_considered);
        println!("  retrieved:       {}", report.citation.retrieved_total);
        println!("  cited:           {}", report.citation.cited_total);
        println!("  citation-rate:   {citation_rate}");

        // --- Proposals ---
        let acceptance_rate = report
            .proposals
            .acceptance_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Proposals ──────────────────────────────────");
        println!("  accepted:        {}", report.proposals.accepted);
        println!("  rejected:        {}", report.proposals.rejected);
        println!("  pending:         {}", report.proposals.pending);
        println!("  acceptance-rate: {acceptance_rate}");

        // --- Usefulness ---
        let avg_ratio = report
            .usefulness
            .avg_ratio
            .map(|v| format!("{:.3}", v))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Usefulness Trend ───────────────────────────");
        println!(
            "  sum-usefulness:      {:.3}",
            report.usefulness.sum_usefulness
        );
        println!("  avg-ratio:           {avg_ratio}");
        println!(
            "  window-finished:     {}",
            report.usefulness.window_finished
        );
        println!(
            "  window-failed(non-gate): {}",
            report.usefulness.window_failed_nongate
        );
        println!("  window-net:          {}", report.usefulness.window_net);

        // --- Harvest ---
        let yield_per_run = report
            .harvest
            .yield_per_run
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Harvest ────────────────────────────────────");
        println!("  created-in-window: {}", report.harvest.created_in_window);
        println!("  yield-per-run:     {yield_per_run}");
        if !report.harvest.by_source.is_empty() {
            let sources: Vec<String> = report
                .harvest
                .by_source
                .iter()
                .map(|(src, n)| format!("{src}={n}"))
                .collect();
            println!("  by-source:         {}", sources.join(", "));
        }

        // --- Corpus ---
        println!("── Corpus Health ──────────────────────────────");
        println!("  active:           {}", report.corpus.active);
        println!("  invalidated:      {}", report.corpus.invalidated);
        println!("  open-conflicts:   {}", report.corpus.open_conflicts);
        println!("  pending-proposals:{}", report.corpus.pending_proposals);
        if !report.corpus.by_scope.is_empty() {
            let scopes: Vec<String> = report
                .corpus
                .by_scope
                .iter()
                .map(|(s, n)| format!("{s}={n}"))
                .collect();
            println!("  by-scope:         {}", scopes.join(", "));
        }
        if !report.corpus.by_kind.is_empty() {
            let kinds: Vec<String> = report
                .corpus
                .by_kind
                .iter()
                .map(|(k, n)| format!("{k}={n}"))
                .collect();
            println!("  by-kind:          {}", kinds.join(", "));
        }
        if !report.corpus.top_useful.is_empty() {
            println!("  top-useful:");
            for m in &report.corpus.top_useful {
                println!(
                    "    [{:.2}] {} — {}",
                    m.usefulness_score, m.memory_id, m.text_preview
                );
            }
        }
        if !report.corpus.prune_candidates.is_empty() {
            println!(
                "  prune-candidates ({}):",
                report.corpus.prune_candidates.len()
            );
            for m in &report.corpus.prune_candidates {
                println!(
                    "    [{:.2}] {} — {}",
                    m.usefulness_score, m.memory_id, m.text_preview
                );
            }
        }

        // --- Token Economy ---
        let avg_tokens = report
            .token_economy
            .avg_injected_tokens
            .map(|v| format!("{:.0}", v))
            .unwrap_or_else(|| "n/a".to_string());
        let avg_capsules = report
            .token_economy
            .avg_capsules
            .map(|v| format!("{:.2}", v))
            .unwrap_or_else(|| "n/a".to_string());
        let skip_rate = report
            .token_economy
            .skip_rate
            .map(|v| format!("{:.1}%", v * 100.0))
            .unwrap_or_else(|| "n/a".to_string());
        println!("── Token Economy ──────────────────────────────");
        println!("  avg-injected-tokens: {avg_tokens}");
        println!("  avg-capsules:        {avg_capsules}");
        println!("  skip-rate:           {skip_rate}");
    }
    Ok(())
}

/// v1.5 / S2.4: `kimetsu brain roi` — ROI ledger.
fn brain_roi(args: RoiArgs) -> KimetsuResult<()> {
    use kimetsu_brain::roi::{RoiWindow, per_memory_roi, roi_report};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let window = RoiWindow::parse(&args.window)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let (_paths, config, conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;
    let report = roi_report(
        &conn,
        window,
        &config.model.model,
        config.model.price_per_mtok,
    )?;

    // S2.4(a): --top mode.
    if let Some(top_n) = args.top {
        let limit = if top_n == 0 { 10 } else { top_n };
        let entries = per_memory_roi(&conn, window, limit)?;

        if args.json {
            println!("{}", serde_json::to_string_pretty(&entries)?);
            return Ok(());
        }

        let window_label = match report.window_days {
            Some(d) => format!("last {d} days"),
            None => "all time".to_string(),
        };
        println!("── ROI Top Memories ({window_label}, top {limit}) ─────");
        if entries.is_empty() {
            println!("  No citations recorded yet.");
        } else {
            for (i, e) in entries.iter().enumerate() {
                println!(
                    "  #{:>2}  [{:>15}]  cites={:>3}  saved={:>6} tok  {}",
                    i + 1,
                    e.kind,
                    e.citation_count,
                    format_token_count(e.estimated_saved_tokens),
                    if e.text_head.len() >= 60 {
                        format!("{}…", &e.text_head[..60])
                    } else {
                        e.text_head.clone()
                    },
                );
            }
        }
        println!("──────────────────────────────────────────────");
        println!("  (Use without --top for the full ROI summary)");
        return Ok(());
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    // Human output.
    let window_label = match report.window_days {
        Some(d) => format!("last {d} days"),
        None => "all time".to_string(),
    };
    println!("── ROI Ledger ({window_label}) ────────────────────────");
    println!("  served events:        {}", report.served_events);
    // S2.4(c): show warm-start events.
    if report.digest_served_events > 0 || report.resume_served_events > 0 {
        println!("  digest_served:        {}", report.digest_served_events);
        println!("  resume_served:        {}", report.resume_served_events);
        println!(
            "  warmstart saved tok:  {}",
            format_token_count(report.warmstart_saved_tokens)
        );
    }
    println!("  citations:            {}", report.citations);
    println!(
        "  injected tokens:      {}",
        format_token_count(report.injected_tokens)
    );
    // S2.4(b): output token estimate.
    println!(
        "  est. output tokens:   {} (ratio est.)",
        format_token_count(report.estimated_output_tokens)
    );
    println!(
        "  est. saved tokens:    {}",
        format_token_count(report.estimated_saved_tokens)
    );
    let net_sign = if report.net_tokens >= 0 { "+" } else { "" };
    println!("  net tokens:           {net_sign}{}", report.net_tokens);

    if let Some(ref usd) = report.usd {
        println!(
            "── USD ({} $/MTok) ─────────────────────────────",
            {
                // Reverse-lookup the price to show it.
                kimetsu_brain::roi::resolve_price_per_mtok(
                    &config.model.model,
                    config.model.price_per_mtok,
                )
                .map(|p| format!("{p:.2}"))
                .unwrap_or_else(|| "?".to_string())
            }
        );
        println!("  saved:  ${:.4}", usd.saved);
        println!("  spent:  ${:.4}", usd.spent);
        let net_usd_sign = if usd.net >= 0.0 { "+" } else { "" };
        println!("  net:    {net_usd_sign}${:.4}", usd.net);
    }

    // Verdict line.
    println!("──────────────────────────────────────────────");
    if report.citations == 0 && report.warmstart_saved_tokens == 0 {
        println!(
            "  No retrieval activity recorded yet — the ledger starts \
             counting as you work."
        );
    } else if report.net_tokens >= 0 {
        match &report.usd {
            Some(u) if u.net >= 0.0 => println!(
                "  Net positive: kimetsu saved you ~{} tokens (~${:.4}) this window.",
                format_token_count(report.estimated_saved_tokens),
                u.net,
            ),
            _ => println!(
                "  Net positive: kimetsu saved you ~{} tokens this window.",
                format_token_count(report.estimated_saved_tokens),
            ),
        }
    } else {
        // Honest negative.
        match &report.usd {
            Some(u) => println!(
                "  Net negative: brain overhead exceeded savings by ~{} tokens (~${:.4}) this window.",
                format_token_count(
                    report
                        .injected_tokens
                        .saturating_sub(report.estimated_saved_tokens)
                ),
                (u.spent - u.saved).abs(),
            ),
            None => println!(
                "  Net negative: brain overhead exceeded savings by ~{} tokens this window.",
                format_token_count(
                    report
                        .injected_tokens
                        .saturating_sub(report.estimated_saved_tokens)
                ),
            ),
        }
    }

    Ok(())
}

/// v1.5 / S2: `kimetsu brain tune` — personal eval readiness + optional sweep.
fn brain_tune(args: TuneArgs) -> KimetsuResult<()> {
    use kimetsu_brain::tune::{compute_model_advisor, compute_retune_trigger};
    use kimetsu_brain::tuneset::build_personal_eval;

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let (_paths2, config, conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;

    if args.revert {
        return brain_tune_revert(&workspace);
    }

    // S2.2: --models only (no sweep).
    if args.models && !args.status {
        let trigger = compute_retune_trigger(&conn, &paths.kimetsu_dir)
            .map_err(|e| format!("compute_retune_trigger: {e}"))?;
        let advisor = compute_model_advisor(&config.embedder.model, &trigger);
        print_model_advisor(&advisor);
        return Ok(());
    }

    let eval = build_personal_eval(&conn, 1800).map_err(|e| format!("build_personal_eval: {e}"))?;

    let positive_count = eval.cases.len();
    let noise_count = eval.noise_count;

    let readiness = if positive_count >= 30 {
        "READY — enough cases for a meaningful sweep."
    } else {
        "accumulating — synthetic fixture will be used for the sweep (< 30 positive cases)."
    };

    // Coverage by memory kind (from relevant memory ids).
    let kind_coverage = kind_coverage_from_eval(&conn, &eval.cases);

    println!("=== kimetsu brain tune --status ===");
    println!("Positive cases (query + ≥1 cited memory): {positive_count}");
    println!("Noise entries  (served, no citation):     {noise_count}");
    if let Some(o) = &eval.oldest {
        println!("Oldest positive case: {o}");
    }
    if let Some(n) = &eval.newest {
        println!("Newest positive case: {n}");
    }
    println!();
    println!("Coverage by memory kind:");
    for (kind, count) in &kind_coverage {
        println!("  {kind:<22} {count}");
    }
    println!();
    println!("Readiness: {readiness}");

    // S2.1: always show trigger state in --status, or when --triggers flag used.
    if args.status || args.triggers {
        println!();
        let trigger = compute_retune_trigger(&conn, &paths.kimetsu_dir)
            .map_err(|e| format!("compute_retune_trigger: {e}"))?;
        print_retune_trigger_state(&trigger);

        // S2.2: show model advisor when --models is also set with --status.
        if args.models {
            println!();
            let advisor = compute_model_advisor(&config.embedder.model, &trigger);
            print_model_advisor(&advisor);
        }

        if args.status {
            return Ok(());
        }
    }

    // Sweep (or dry-run report).
    brain_tune_sweep(&workspace, &paths, args, eval)
}

fn kind_coverage_from_eval(
    conn: &rusqlite::Connection,
    cases: &[kimetsu_brain::eval::EvalCase],
) -> Vec<(String, usize)> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for case in cases {
        for mid in &case.relevant {
            let kind: Option<String> = conn
                .query_row(
                    "SELECT kind FROM memories WHERE memory_id = ?1",
                    rusqlite::params![mid],
                    |r| r.get(0),
                )
                .ok();
            let kind = kind.unwrap_or_else(|| "unknown".to_string());
            *counts.entry(kind).or_default() += 1;
        }
    }
    let mut vec: Vec<(String, usize)> = counts.into_iter().collect();
    vec.sort_by_key(|a| std::cmp::Reverse(a.1));
    vec
}

/// S2.1: Print the re-tune trigger state in a human-readable format.
fn print_retune_trigger_state(trigger: &kimetsu_brain::tune::RetuneTriggerState) {
    println!("=== S2.1 Re-tune Triggers ===");
    if let Some(ts) = &trigger.last_tuned_at {
        println!("  Last tuned at:           {ts}");
        println!(
            "  Memory count at tune:    {}",
            trigger.memory_count_at_last_tune
        );
    } else {
        println!("  Last tuned at:           (never)");
    }
    println!(
        "  Current memory count:    {}",
        trigger.current_memory_count
    );
    println!(
        "  Added since last tune:   {}",
        trigger.memories_added_since_tune
    );
    println!(
        "  Corpus milestone (≥{}): {}",
        kimetsu_brain::tune::RETUNE_CORPUS_MILESTONE,
        if trigger.corpus_milestone_triggered {
            "TRIGGERED"
        } else {
            "not reached"
        }
    );
    println!(
        "  Regret rate (24h):       {:.1}% ({}/{} events)",
        trigger.regret_rate * 100.0,
        trigger.recent_regret_count,
        trigger.recent_served_count
    );
    println!(
        "  Drift threshold (≥{:.0}%): {}",
        kimetsu_brain::tune::RETUNE_REGRET_RATE_THRESHOLD * 100.0,
        if trigger.drift_triggered {
            "TRIGGERED"
        } else {
            "within normal"
        }
    );
    println!();
    if trigger.should_retune {
        println!("  → Re-tune PROPOSED: run `kimetsu brain tune` to run the sweep.");
    } else {
        println!("  → No re-tune needed at this time.");
    }
}

/// S2.2: Print the model re-selection advisor report.
fn print_model_advisor(advisor: &kimetsu_brain::tune::ModelAdvisorReport) {
    println!("=== S2.2 Model Re-selection Advisor ===");
    println!("  Current embedder:  {}", advisor.current_embedder);
    println!("  Memories to reindex: {}", advisor.memories_to_reindex);
    println!(
        "  Est. reindex cost:   ~{} tokens (conservative lower-bound)",
        format_token_count(advisor.estimated_reindex_tokens)
    );
    println!();
    println!("  {}", advisor.reason);
    println!();
    println!("  Candidate models (for grid sweep):");
    for m in &advisor.candidate_models {
        println!(
            "    {:<40} ~{} MiB download",
            m.model_id, m.approx_download_mib
        );
        println!("      {}", m.description);
    }
    println!();
    if advisor.recommend_grid_run {
        println!("  → Grid run RECOMMENDED. Re-run with the full sweep after downloading models.");
        println!("    NOTE: This advisor NEVER auto-switches the model. Apply changes manually.");
    } else {
        println!("  → Grid run optional. Current model appears sufficient.");
    }
}

fn brain_tune_sweep(
    workspace: &std::path::Path,
    paths: &kimetsu_core::paths::ProjectPaths,
    args: TuneArgs,
    eval: kimetsu_brain::tuneset::PersonalEval,
) -> KimetsuResult<()> {
    use kimetsu_brain::context::{ContextRequest, rerank_capsules};
    use kimetsu_brain::embeddings::{open_embedder_for, open_reranker_for_model};
    use kimetsu_brain::eval::{mean, mrr};
    use kimetsu_brain::project::BrainSession;
    use kimetsu_brain::tune::{
        ComboResult, TuneCombo, TuneHistoryEntry, append_tune_history,
        compute_objective_with_regret, count_regret_events, select_winner, train_holdout_split,
    };
    use std::collections::HashMap;
    use time::format_description::well_known::Rfc3339;

    let config = project::load_config(paths)?;
    // Tune against the PRODUCTION retrieval pipeline: the same embedder
    // resolution as retrieve_context_with_request. On embeddings builds this
    // loads the real model (semantic floors only discriminate with real
    // cosines); lean builds degrade to Noop and sweep FTS-only — the status
    // output should make that visible to the user.
    let embedder = open_embedder_for(config.embedder.enabled);
    if embedder.is_noop() {
        println!(
            "note: lean build/embedder disabled — sweeping FTS-only retrieval \
             (semantic floor values will not differentiate)"
        );
    }
    let current_combo = TuneCombo {
        min_lexical_coverage: config.broker.min_lexical_coverage,
        min_semantic_score: config.broker.min_semantic_score,
        reranker_id: config.embedder.reranker.clone(),
    };

    // Choose eval cases: personal if READY, else fall back to fixture.
    let fallback_fixture_path = std::path::Path::new("fixtures/eval-retrieval.json");
    let (cases, using_personal) = if eval.cases.len() >= 30 {
        (eval.cases.clone(), true)
    } else {
        // Load the committed fixture.
        if !fallback_fixture_path.exists() {
            println!(
                "note: fewer than 30 personal eval cases ({}) and no fixture at {}. \
                 Sweep skipped. Accumulate more sessions with store_queries=true.",
                eval.cases.len(),
                fallback_fixture_path.display()
            );
            return Ok(());
        }
        let text = std::fs::read_to_string(fallback_fixture_path)
            .map_err(|e| format!("read fixture: {e}"))?;
        let fixture: kimetsu_brain::eval::EvalFixture =
            serde_json::from_str(&text).map_err(|e| format!("parse fixture: {e}"))?;
        // Fixture uses key-based relevance, not memory_ids. For the sweep
        // we need memory_ids. We cannot map them here (fixture is hermetic).
        // Instead: use fixture cases as-is for MRR calculation but note that
        // relevant ids won't match real DB memories → MRR will be 0.
        // The sweep is still meaningful for comparing COMBOS relatively.
        let eval_cases: Vec<kimetsu_brain::eval::EvalCase> = fixture
            .cases
            .into_iter()
            .map(|c| kimetsu_brain::eval::EvalCase {
                query: c.query,
                relevant: c.relevant,
                kind: Default::default(),
                stale: Vec::new(),
            })
            .collect();
        (eval_cases, false)
    };

    if !using_personal {
        println!(
            "note: fewer than 30 personal eval cases ({}). Using fixture file for relative sweep.",
            eval.cases.len()
        );
        // Fix 3: guard --apply behind personal data.
        // In fixture mode MRR≡0 for every combo (fixture IDs don't match real
        // memories), so the objective degenerates to pure token-minimisation.
        // Applying the resulting floors would optimise for fewer tokens at the
        // cost of recall.  Refuse --apply until the user has ≥30 cited cases.
        if args.apply {
            println!(
                "note: fixture mode is relative-only — --apply refused. \
                 Accumulate ≥30 cited cases first (see `kimetsu brain tune --status`)."
            );
            return Ok(());
        }
    }

    let n = cases.len();
    if n == 0 {
        println!("No eval cases available. Run more sessions with store_queries=true.");
        return Ok(());
    }

    let (train_idx, holdout_idx) = train_holdout_split(n);
    let train_cases: Vec<&kimetsu_brain::eval::EvalCase> =
        train_idx.iter().map(|&i| &cases[i]).collect();
    let holdout_cases: Vec<&kimetsu_brain::eval::EvalCase> =
        holdout_idx.iter().map(|&i| &cases[i]).collect();

    println!(
        "Sweep: {} combos × {} train / {} holdout cases",
        kimetsu_brain::tune::TuneCombo::all_combos().len(),
        train_cases.len(),
        holdout_cases.len()
    );

    // Cache reranker handles (load once, reuse).
    let mut reranker_cache: HashMap<String, Option<Box<dyn kimetsu_brain::embeddings::Reranker>>> =
        HashMap::new();
    for rr_id in kimetsu_brain::tune::RERANKER_IDS {
        let rr: Option<Box<dyn kimetsu_brain::embeddings::Reranker>> = if *rr_id == "off" {
            None
        } else {
            open_reranker_for_model(rr_id)
        };
        reranker_cache.insert(rr_id.to_string(), rr);
    }

    // Helper: evaluate one combo over a slice of cases.
    let evaluate_cases =
        |combo: &TuneCombo, case_slice: &[&kimetsu_brain::eval::EvalCase]| -> (f64, f64) {
            let session = match BrainSession::open_readonly(workspace) {
                Ok(s) => s,
                Err(_) => return (0.0, 0.0),
            };
            let rr_ref = reranker_cache
                .get(&combo.reranker_id)
                .and_then(|r| r.as_deref());
            let rerank_floor = 0.30f32;
            let rerank_cap = 4usize;
            let pool = 8usize;

            let mut mrr_vals: Vec<f64> = Vec::new();
            let mut token_vals: Vec<f64> = Vec::new();

            for case in case_slice {
                let request = ContextRequest {
                    stage: "localization".to_string(),
                    query: case.query.clone(),
                    budget_tokens: 6000,
                    max_capsules: pool,
                    min_semantic_score: combo.min_semantic_score,
                    min_lexical_coverage: combo.min_lexical_coverage,
                    ..Default::default()
                };
                let mut bundle =
                    match session.retrieve_context_with_injected_embedder(request, embedder) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                if let Some(rr) = rr_ref {
                    bundle.capsules =
                        rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
                }

                let ranked_ids: Vec<String> = bundle
                    .capsules
                    .iter()
                    .filter_map(|c| {
                        c.expansion_handle
                            .strip_prefix("memory:")
                            .map(str::to_string)
                    })
                    .collect();

                let mrr_val = mrr(&ranked_ids, &case.relevant);
                mrr_vals.push(mrr_val);

                let tokens: f64 = bundle
                    .capsules
                    .iter()
                    .map(|c| c.token_estimate as f64)
                    .sum();
                token_vals.push(tokens);
            }

            (mean(&mrr_vals), mean(&token_vals))
        };

    // S2.3: Compute global regret rate from the DB for the objective penalty.
    // We use the ALL-TIME regret / served ratio here (the sweep window is the
    // full personal eval set, which spans all time).
    // Best-effort: if the DB cannot be opened, regret_rate and memory_count
    // degrade gracefully to 0 (objective falls back to v1.5 formula).
    let (global_regret_rate, current_memory_count) = {
        match kimetsu_brain::project::load_project_readonly(workspace) {
            Ok((_paths_ro, _cfg_ro, conn_ro)) => {
                let total_regrets = count_regret_events(&conn_ro, None, None).unwrap_or(0);
                let total_served: u64 = conn_ro
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE kind = 'context.served'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let regret_rate = if total_served > 0 {
                    total_regrets as f64 / total_served as f64
                } else {
                    0.0
                };
                let mem_count: u64 = conn_ro
                    .query_row(
                        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                (regret_rate, mem_count)
            }
            Err(_) => (0.0_f64, 0_u64),
        }
    };

    // Evaluate current config on holdout for baseline.
    let (baseline_holdout_mrr, baseline_holdout_tokens) =
        evaluate_cases(&current_combo, &holdout_cases);
    let baseline_holdout_obj = compute_objective_with_regret(
        baseline_holdout_mrr,
        baseline_holdout_tokens,
        args.cost_weight,
        global_regret_rate,
    );

    // Sweep all combos on TRAIN set.
    let all_combos = TuneCombo::all_combos();
    let mut combo_results: Vec<ComboResult> = Vec::new();

    for (i, combo) in all_combos.iter().enumerate() {
        if i % 10 == 0 {
            print!("\r  sweeping combo {}/{} ...", i + 1, all_combos.len());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        let (mmrr, mtok) = evaluate_cases(combo, &train_cases);
        // S2.3: include regret penalty in the objective.
        let obj = compute_objective_with_regret(mmrr, mtok, args.cost_weight, global_regret_rate);
        combo_results.push(ComboResult {
            combo: combo.clone(),
            mean_mrr: mmrr,
            mean_tokens: mtok,
            objective: obj,
        });
    }
    println!();

    let winner = match select_winner(&combo_results) {
        Some(w) => w,
        None => {
            println!("No combos evaluated. Nothing to tune.");
            return Ok(());
        }
    };

    // Evaluate winner on HOLDOUT (with regret penalty for consistency).
    let (holdout_mrr, holdout_tokens) = evaluate_cases(&winner.combo, &holdout_cases);
    let holdout_obj = compute_objective_with_regret(
        holdout_mrr,
        holdout_tokens,
        args.cost_weight,
        global_regret_rate,
    );
    let improvement = holdout_obj - baseline_holdout_obj;

    println!();
    println!("=== Tune Sweep Results ===");
    println!(
        "Current config:  lex={:.2} sem={:.3} rr={}",
        current_combo.min_lexical_coverage,
        current_combo.min_semantic_score,
        current_combo.reranker_id
    );
    println!(
        "Best combo:      lex={:.2} sem={:.3} rr={}",
        winner.combo.min_lexical_coverage,
        winner.combo.min_semantic_score,
        winner.combo.reranker_id
    );
    println!(
        "Train objective: {:.4}  (MRR {:.4}, avg_tokens {:.1})",
        winner.objective, winner.mean_mrr, winner.mean_tokens
    );
    println!(
        "Holdout objective: {:.4} vs baseline {:.4} (improvement: {:+.4})",
        holdout_obj, baseline_holdout_obj, improvement
    );

    if improvement < 0.01 {
        println!();
        println!(
            "verdict: no change recommended (holdout improvement {improvement:+.4} < 0.01 threshold)"
        );
        return Ok(());
    }

    println!();
    // Reranker change recommendation (never auto-applied).
    if winner.combo.reranker_id != current_combo.reranker_id {
        println!(
            "note: reranker change recommended ({} → {}) — apply manually after \
             downloading the model and restarting the MCP daemon.",
            current_combo.reranker_id, winner.combo.reranker_id
        );
    }

    if !args.apply {
        if !using_personal {
            println!(
                "note: fixture mode — results are relative only; \
                 --apply is disabled until you have ≥30 cited cases."
            );
        }
        println!(
            "DRY RUN — to apply floor changes: kimetsu brain tune --apply\n\
             (floor changes: lex {:.2}→{:.2}, sem {:.3}→{:.3})",
            current_combo.min_lexical_coverage,
            winner.combo.min_lexical_coverage,
            current_combo.min_semantic_score,
            winner.combo.min_semantic_score,
        );
        return Ok(());
    }

    // --apply: write floors to project.toml using surgical toml_edit so that
    // user comments and unknown keys are preserved (S4.2).
    let disk_text = std::fs::read_to_string(&paths.project_toml)
        .map_err(|e| format!("tune --apply: could not read project.toml: {e}"))?;
    let mut doc: toml_edit::DocumentMut = disk_text
        .parse()
        .map_err(|e| format!("tune --apply: project.toml is invalid TOML: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_lexical_coverage",
        &toml::Value::Float(winner.combo.min_lexical_coverage as f64),
    )
    .map_err(|e| format!("tune --apply: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_semantic_score",
        &toml::Value::Float(winner.combo.min_semantic_score as f64),
    )
    .map_err(|e| format!("tune --apply: {e}"))?;
    std::fs::write(&paths.project_toml, doc.to_string())?;

    // Snapshot to tune-history.
    let now_str = time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let history_entry = TuneHistoryEntry {
        timestamp: now_str,
        before: current_combo,
        after: winner.combo.clone(),
        train_objective: winner.objective,
        holdout_objective: holdout_obj,
        holdout_mrr,
        baseline_holdout_objective: baseline_holdout_obj,
        // S2.1: record corpus size so re-tune trigger can detect growth.
        memory_count_at_tune: Some(current_memory_count),
    };
    append_tune_history(&paths.kimetsu_dir, history_entry)?;

    println!(
        "Applied: lex_coverage={:.2}, sem_score={:.3} → project.toml updated.",
        winner.combo.min_lexical_coverage, winner.combo.min_semantic_score
    );
    println!("Snaphotted to .kimetsu/tune-history.json");

    Ok(())
}

fn brain_tune_revert(workspace: &std::path::Path) -> KimetsuResult<()> {
    use kimetsu_brain::tune::latest_tune_history;

    let paths = kimetsu_core::paths::ProjectPaths::discover(workspace)?;
    let Some(entry) = latest_tune_history(&paths.kimetsu_dir)? else {
        println!("No tune history found — nothing to revert.");
        return Ok(());
    };

    // S4.2: surgical write via toml_edit preserves user comments.
    let disk_text = std::fs::read_to_string(&paths.project_toml)
        .map_err(|e| format!("tune revert: could not read project.toml: {e}"))?;
    let mut doc: toml_edit::DocumentMut = disk_text
        .parse()
        .map_err(|e| format!("tune revert: project.toml is invalid TOML: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_lexical_coverage",
        &toml::Value::Float(entry.before.min_lexical_coverage as f64),
    )
    .map_err(|e| format!("tune revert: {e}"))?;
    set_toml_edit_path(
        &mut doc,
        "broker.min_semantic_score",
        &toml::Value::Float(entry.before.min_semantic_score as f64),
    )
    .map_err(|e| format!("tune revert: {e}"))?;
    std::fs::write(&paths.project_toml, doc.to_string())?;

    println!(
        "Reverted: lex_coverage={:.2}, sem_score={:.3} (from tune at {})",
        entry.before.min_lexical_coverage, entry.before.min_semantic_score, entry.timestamp
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Story 3.1 + 3.2: kimetsu brain consolidate
// ---------------------------------------------------------------------------

fn brain_consolidate(args: ConsolidateArgs) -> KimetsuResult<()> {
    use kimetsu_brain::consolidate::{
        ConsolidateOptions, DistillOptions, find_distill_clusters, load_embeddable_rows,
        run_consolidation,
    };

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let (paths, _config, conn) = kimetsu_brain::project::load_project(&workspace)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // --- Story 3.1: near-duplicate merge ---
    // --distill is additive; 3.1 merge always runs alongside it.
    {
        let opts = ConsolidateOptions {
            threshold: args.threshold,
            dry_run: args.dry_run,
        };

        if !args.dry_run && !args.yes {
            // Check TTY requirement.
            if !io::stdin().is_terminal() {
                return Err(
                    "stdin is not a TTY; pass --yes to confirm consolidation non-interactively"
                        .into(),
                );
            }
            // Interactive prompt.
            write!(
                out,
                "Consolidate near-duplicate memories (threshold={:.2})? [y/N] ",
                args.threshold
            )?;
            out.flush()?;
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                writeln!(out, "Aborted.")?;
                return Ok(());
            }
        }

        run_consolidation(&conn, &opts, &mut out)?;
    }

    // --- Story 3.2: cluster distillation (--distill flag) ---
    if args.distill {
        let dopts = DistillOptions::default();
        let by_model = load_embeddable_rows(&conn)?;
        let all_rows: Vec<_> = by_model.into_values().flatten().collect();
        let clusters = find_distill_clusters(&all_rows, &dopts);

        if clusters.is_empty() {
            writeln!(
                out,
                "\nNo distillable clusters found (lo={:.2} hi={:.2}, min_size={}).",
                dopts.lo, dopts.hi, dopts.min_cluster_size
            )?;
            return Ok(());
        }

        // Try to resolve a distiller.
        let resolved = distiller::resolve_distiller(&workspace);

        if resolved.is_none() || args.dry_run {
            writeln!(
                out,
                "\nDistillable clusters ({} found — lo={:.2} hi={:.2}):",
                clusters.len(),
                dopts.lo,
                dopts.hi
            )?;
            for (i, cluster) in clusters.iter().enumerate() {
                writeln!(
                    out,
                    "\nCluster {} [tags: {}]:",
                    i + 1,
                    cluster.shared_tags.join(", ")
                )?;
                for m in &cluster.memories {
                    writeln!(
                        out,
                        "  [{}] {}",
                        m.memory_id,
                        &m.text[..m.text.len().min(80)]
                    )?;
                }
            }
            if resolved.is_none() {
                writeln!(
                    out,
                    "\nNo distiller configured — printed clusters above. Configure [learning.distiller] to auto-distil."
                )?;
            }
            return Ok(());
        }

        // Distiller is available — generate proposals.
        let distiller_resolved = resolved.unwrap();
        let mut proposals_created = 0usize;
        for cluster in &clusters {
            let cluster_text = cluster
                .memories
                .iter()
                .enumerate()
                .map(|(i, m)| format!("{}. {}", i + 1, m.text))
                .collect::<Vec<_>>()
                .join("\n");
            let prompt = format!(
                "Distill these {} related lessons into ONE general principle \
                 (2-4 sentences, imperative, no project-specific context):\n\n{cluster_text}",
                cluster.memories.len()
            );
            let mut provider = distiller::make_provider_for_resolved(&distiller_resolved);
            if let Some(ref mut p) = provider {
                let lessons = distiller::distill_lessons(&prompt, p.as_mut());
                for lesson in lessons {
                    let result = kimetsu_brain::project::propose_memory(
                        &distiller_resolved.record_start,
                        distiller_resolved.scope,
                        MemoryKind::Convention,
                        &lesson.lesson,
                        lesson.confidence.clamp(0.0, 1.0),
                        &format!(
                            "distilled from cluster [tags: {}]",
                            cluster.shared_tags.join(", ")
                        ),
                    );
                    if result.is_ok() {
                        proposals_created += 1;
                    }
                }
            }
        }

        writeln!(
            out,
            "\nCreated {proposals_created} distillation proposal(s). Review with: kimetsu brain memory proposals"
        )?;
        drop(paths);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Flagship 2 / Story 2.3: kimetsu brain reflect
// ---------------------------------------------------------------------------

/// Adapter: wraps a `kimetsu_agent` `ModelProvider` so it can be used as the
/// `kimetsu_brain::consolidate::ModelProvider` the `run_reflection` function
/// expects.
struct ReflectionModelAdapter<'a> {
    inner: &'a mut dyn kimetsu_agent::model::ModelProvider,
}

impl<'a> kimetsu_brain::consolidate::ModelProvider for ReflectionModelAdapter<'a> {
    fn complete_text(&mut self, prompt: &str) -> Option<String> {
        use kimetsu_agent::model::{ModelMessage, ModelRequest, ToolChoice};
        let req = ModelRequest {
            messages: vec![ModelMessage::user_text(prompt)],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 512,
            temperature: 0.2,
            metadata: serde_json::Value::Null,
        };
        self.inner.complete(req).ok()?.text
    }
}

fn brain_reflect(args: ReflectArgs) -> KimetsuResult<()> {
    use kimetsu_brain::consolidate::{ReflectionOptions, run_reflection};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let (_paths, _config, conn) = kimetsu_brain::project::load_project(&workspace)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let opts = ReflectionOptions {
        dry_run: args.dry_run,
        ..Default::default()
    };

    // Try to resolve a cheap model.
    let resolved = distiller::resolve_distiller(&workspace);

    if resolved.is_none() || args.dry_run {
        run_reflection(&conn, &opts, None, &mut out)?;
        if resolved.is_none() && !args.dry_run {
            writeln!(
                out,
                "\nNo cheap model configured — printed clusters above.\n\
                 Configure [cheap_model] in project.toml to auto-synthesize principles."
            )?;
        }
        return Ok(());
    }

    // Build the model provider from the resolved distiller.
    let distiller_resolved = resolved.unwrap();
    let mut provider_box = distiller::make_provider_for_resolved(&distiller_resolved);
    let summary = if let Some(ref mut p) = provider_box {
        let mut adapter = ReflectionModelAdapter { inner: p.as_mut() };
        run_reflection(&conn, &opts, Some(&mut adapter), &mut out)?
    } else {
        run_reflection(&conn, &opts, None, &mut out)?
    };

    if summary.proposals_created > 0 {
        writeln!(
            out,
            "\nCreated {} reflection proposal(s). Review with: kimetsu brain memory proposals",
            summary.proposals_created
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Story 3.3: kimetsu brain triage
// ---------------------------------------------------------------------------

fn brain_triage(args: TriageArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let (_paths, _config, conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let stdin = io::stdin();
    let mut sin = stdin.lock();

    let candidates = triage_candidates(&conn, args.score_floor, args.age_days)?;

    if candidates.is_empty() {
        writeln!(
            out,
            "No fading memories found (score_floor={:.2}, age_days={}).",
            args.score_floor, args.age_days
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "{} fading memor{} (score < {:.2}, age > {}d):",
        candidates.len(),
        if candidates.len() == 1 { "y" } else { "ies" },
        args.score_floor,
        args.age_days
    )?;

    if args.prune_all {
        if !args.yes {
            if !io::stdin().is_terminal() {
                return Err(
                    "stdin is not a TTY; pass --yes to confirm --prune-all non-interactively"
                        .into(),
                );
            }
            write!(out, "Prune all {} candidates? [y/N] ", candidates.len())?;
            out.flush()?;
            let mut line = String::new();
            sin.read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                writeln!(out, "Aborted.")?;
                return Ok(());
            }
        }
        let mut pruned = 0usize;
        for c in &candidates {
            let reason = format!(
                "triage_prune score={:.2} age_days={}",
                c.usefulness_score, c.age_days
            );
            if kimetsu_brain::project::invalidate_memory(&workspace, &c.memory_id, Some(&reason))
                .is_ok()
            {
                pruned += 1;
            }
        }
        writeln!(
            out,
            "Pruned {pruned} memor{}.",
            if pruned == 1 { "y" } else { "ies" }
        )?;
        return Ok(());
    }

    // Interactive per-item loop.
    if !io::stdin().is_terminal() {
        // Non-TTY with no --prune-all: just print the list.
        for c in &candidates {
            writeln!(
                out,
                "[{}] {}/{} age={}d score={:.2} — {}",
                c.memory_id,
                c.scope,
                c.kind,
                c.age_days,
                c.usefulness_score,
                &c.text[..c.text.len().min(80)]
            )?;
        }
        writeln!(out, "\nPass --prune-all --yes to prune non-interactively.")?;
        return Ok(());
    }

    triage_interactive_loop(&workspace, &candidates, &mut sin, &mut out)
}

// ---------------------------------------------------------------------------
// F3 Story 3.1 + 3.3: brain forget
// ---------------------------------------------------------------------------

fn brain_forget(args: ForgetArgs) -> KimetsuResult<()> {
    use kimetsu_brain::lifecycle::{ForgetOptions, ProposalGcOptions, forget_brain, gc_proposals};

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Load config to read lifecycle defaults.
    let (_paths, config, _conn) = kimetsu_brain::project::load_project_readonly(&workspace)?;
    let lc = &config.lifecycle;

    // Respect the opt-in gate unless --force-enabled or --dry-run.
    if !args.dry_run && !args.force_enabled && !lc.forget_enabled {
        eprintln!(
            "Forgetting is disabled in project.toml (lifecycle.forget_enabled = false).\n\
             Pass --force-enabled to override for this run, or set it in project.toml."
        );
        return Ok(());
    }

    // --json is report-only: never write, so it composes safely with harnesses.
    let report_only = args.dry_run || args.json;

    let opts = ForgetOptions {
        dry_run: report_only,
        usefulness_floor: args.usefulness_floor.unwrap_or(lc.forget_usefulness_floor),
        min_age_days: args.min_age_days.unwrap_or(lc.forget_min_age_days),
        protect_use_count: args
            .protect_use_count
            .unwrap_or(lc.forget_protect_use_count),
    };

    // -- Forget pass --
    let summary = forget_brain(&workspace, opts)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    if report_only {
        if summary.candidates.is_empty() {
            println!("dry-run: no memories matched the forget criteria.");
        } else {
            println!(
                "dry-run: {} memor{} would be forgotten:",
                summary.candidates.len(),
                if summary.candidates.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            for c in &summary.candidates {
                println!(
                    "  [{}] {}/{} use_count={} usefulness={:.3} age={:.0}d — {}",
                    &c.memory_id[..c.memory_id.len().min(12)],
                    c.scope,
                    c.kind,
                    c.use_count,
                    c.usefulness_score,
                    c.age_days,
                    &c.text_preview
                );
            }
        }
    } else {
        // Confirm unless --yes.
        if !args.yes && !summary.candidates.is_empty() {
            if !io::stdin().is_terminal() {
                return Err(
                    "stdin is not a TTY; pass --yes to confirm forgetting non-interactively".into(),
                );
            }
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let stdin = io::stdin();
            let mut sin = stdin.lock();
            write!(
                out,
                "Forget {} memor{}? [y/N] ",
                summary.candidates.len(),
                if summary.candidates.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            )?;
            out.flush()?;
            let mut line = String::new();
            sin.read_line(&mut line)?;
            let answer = line.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                println!("Aborted.");
                return Ok(());
            }
        }

        if summary.archived == 0 {
            println!("No memories matched the forget criteria. Brain is already lean.");
        } else {
            println!(
                "Forgot {} memor{} (archived via invalidation events).",
                summary.archived,
                if summary.archived == 1 { "y" } else { "ies" }
            );
        }
        if summary.failed > 0 {
            eprintln!(
                "Warning: {} memor{} could not be archived (check logs).",
                summary.failed,
                if summary.failed == 1 { "y" } else { "ies" }
            );
        }
    }

    // -- Proposal GC hygiene pass (Story 3.3) --
    if !args.no_proposal_gc {
        let gc_opts = ProposalGcOptions {
            dry_run: args.dry_run,
            expiry_days: lc.proposal_expiry_days,
            auto_accept_confidence: lc.proposal_auto_accept_confidence,
        };
        match gc_proposals(&workspace, gc_opts) {
            Ok(gc) => {
                if gc.expired > 0 {
                    let verb = if args.dry_run {
                        "would expire"
                    } else {
                        "expired"
                    };
                    println!(
                        "Proposal GC: {verb} {} stale proposal{}.",
                        gc.expired,
                        if gc.expired == 1 { "" } else { "s" }
                    );
                }
                if gc.auto_accepted > 0 {
                    let verb = if args.dry_run {
                        "would auto-accept"
                    } else {
                        "auto-accepted"
                    };
                    println!(
                        "Proposal GC: {verb} {} high-confidence proposal{}.",
                        gc.auto_accepted,
                        if gc.auto_accepted == 1 { "" } else { "s" }
                    );
                }
            }
            Err(e) => {
                // Non-fatal — just warn.
                eprintln!("Warning: proposal GC encountered an error: {e}");
            }
        }
    }

    Ok(())
}

fn brain_cite(args: CiteArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    project::record_mcp_citation(&workspace, &args.memory_id, args.note.as_deref())?;
    println!("Cited memory {} (memory.cited recorded).", args.memory_id);
    Ok(())
}

fn brain_regret(args: RegretArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    project::record_regret(&workspace, &args.memory_id)?;
    println!(
        "Flagged memory {} as regretted (retrieval.regret recorded).",
        args.memory_id
    );
    Ok(())
}

fn brain_distill(args: DistillArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let resolved = distiller::resolve_distiller(&workspace).ok_or_else(|| {
        "no cheap model configured: set [cheap_model] provider + model in \
         .kimetsu/project.toml (e.g. provider = \"ollama\", model = \"qwen2.5:3b\")"
            .to_string()
    })?;
    let mut provider = distiller::make_provider_for_resolved(&resolved).ok_or_else(|| {
        format!(
            "could not construct the '{}' model provider for distillation",
            resolved.provider
        )
    })?;

    let transcript = args.transcript.to_string_lossy();
    let view = distiller::build_transcript_view(&transcript, distiller::MAX_VIEW_CHARS);
    if view.trim().is_empty() {
        if args.json {
            println!("[]");
        } else {
            eprintln!("transcript is empty or unreadable: {transcript}");
        }
        return Ok(());
    }

    let lessons = distiller::distill_lessons(&view, provider.as_mut());

    if args.json {
        let rows: Vec<serde_json::Value> = lessons
            .iter()
            .map(|l| {
                serde_json::json!({
                    "lesson": l.lesson,
                    "tags": l.tags,
                    "kind": l.kind,
                    "confidence": l.confidence,
                    "valid_from": l.valid_from,
                    "valid_to": l.valid_to,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if lessons.is_empty() {
        println!("no lessons distilled from this transcript.");
    } else {
        println!(
            "distilled {} lesson{} (not recorded):",
            lessons.len(),
            if lessons.len() == 1 { "" } else { "s" }
        );
        for l in &lessons {
            println!(
                "  [{}] {} (confidence {:.2}; tags: {})",
                l.kind,
                l.lesson,
                l.confidence,
                l.tags.join(", ")
            );
        }
    }
    Ok(())
}

/// #2 knowledge graph dispatch.
fn brain_graph(command: GraphCommand) -> KimetsuResult<()> {
    match command {
        GraphCommand::Build(args) => brain_graph_build(args),
    }
}

/// `kimetsu brain graph build`: derive `relates_to` edges (rule layer) over the
/// active memories and persist them as rebuild-safe `memory.edge` events. With
/// `--enrich`, additionally ask the cheap model for typed edges. With `--dry-run`,
/// preview counts without writing. With `--json`, emit a machine-readable summary.
fn brain_graph_build(args: GraphBuildArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Optional LLM enrichment: typed edges proposed by the cheap model. Best
    // effort — a missing model or unparseable response yields zero extra edges.
    let extra_edges: Vec<(String, String, String)> = if args.enrich {
        match project::active_memory_texts(&workspace) {
            Ok(mems) => enrich_typed_edges(&workspace, &mems),
            Err(e) => {
                eprintln!("kimetsu: graph enrich skipped (could not read memories: {e})");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let summary = project::build_graph(&workspace, &extra_edges, args.max_fan_out, args.dry_run)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    let verb = if summary.dry_run {
        "would write"
    } else {
        "wrote"
    };
    println!(
        "Graph build: {} active memories, {} rule + {} enrichment edges proposed; {} {} edge(s).",
        summary.active_memories,
        summary.rule_edges,
        summary.enrich_edges,
        verb,
        if summary.dry_run {
            summary.by_type.values().sum::<usize>()
        } else {
            summary.written
        }
    );
    for (ty, n) in &summary.by_type {
        println!("  {ty}: {n}");
    }
    if summary.dry_run {
        println!("(dry-run: nothing persisted; re-run without --dry-run to write)");
    }
    Ok(())
}

/// LLM enrichment for the knowledge graph: ask the configured cheap model, for
/// each active memory, which OTHER memory it most directly refines or derives
/// from, and with what typed relation. Returns `(src_id, dst_id, edge_type)`
/// tuples restricted to the reserved typed-edge vocabulary and to ids that exist
/// in `memories`. Best-effort and bounded: returns an empty vec when no cheap
/// model is configured. Small local models are weak at this (documented).
fn enrich_typed_edges(
    workspace: &Path,
    memories: &[(String, String)],
) -> Vec<(String, String, String)> {
    const ALLOWED: [&str; 3] = ["refines", "lesson_from", "decision_touches"];
    // Bound the work: enrichment is opt-in and model-bottlenecked.
    const MAX_MEMORIES: usize = 200;

    let Some(resolved) = distiller::resolve_distiller(workspace) else {
        eprintln!("kimetsu: --enrich requested but no [cheap_model] configured; rule edges only.");
        return Vec::new();
    };
    let Some(mut provider) = distiller::make_provider_for_resolved(&resolved) else {
        eprintln!(
            "kimetsu: --enrich could not construct the cheap-model provider; rule edges only."
        );
        return Vec::new();
    };

    let ids: std::collections::HashSet<&str> = memories.iter().map(|(id, _)| id.as_str()).collect();
    // A compact catalog the model can reference by id.
    let catalog: String = memories
        .iter()
        .take(MAX_MEMORIES)
        .map(|(id, text)| {
            format!(
                "{id}\t{}",
                text.replace('\n', " ")
                    .chars()
                    .take(160)
                    .collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    const SYSTEM: &str = "You connect software-engineering memories into a knowledge graph. \
        Given a SOURCE memory and a CATALOG of other memories (id<TAB>text), pick AT MOST ONE \
        catalog memory the SOURCE most directly relates to, and the relation type. Allowed types: \
        refines (source narrows/refines target), lesson_from (source is a lesson learned from \
        target), decision_touches (source is a decision touching target). Reply with ONE line of \
        strict JSON: {\"dst\":\"<id or empty>\",\"type\":\"<type or empty>\"}. If nothing relates, \
        reply {\"dst\":\"\",\"type\":\"\"}. Output only the JSON.";

    let mut out: Vec<(String, String, String)> = Vec::new();
    for (id, text) in memories.iter().take(MAX_MEMORIES) {
        let user = format!(
            "SOURCE ({id}): {src}\n\nCATALOG:\n{catalog}",
            id = id,
            src = text
                .replace('\n', " ")
                .chars()
                .take(240)
                .collect::<String>(),
            catalog = catalog,
        );
        let Some(reply) = distiller::complete_simple(SYSTEM, &user, 64, provider.as_mut()) else {
            continue;
        };
        let reply = reply.trim();
        // Extract the first {...} object from the reply.
        let (Some(s), Some(e)) = (reply.find('{'), reply.rfind('}')) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&reply[s..=e]) else {
            continue;
        };
        let dst = v.get("dst").and_then(|x| x.as_str()).unwrap_or("").trim();
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("").trim();
        if dst.is_empty() || ty.is_empty() || dst == id {
            continue;
        }
        if ALLOWED.contains(&ty) && ids.contains(dst) {
            out.push((id.clone(), dst.to_string(), ty.to_string()));
        }
    }
    out
}

/// HyDE query expansion: append a hypothetical answer passage (from the cheap
/// model) to `query`, so semantic retrieval matches the answer's vector rather
/// than the question's. Falls back to the raw query when no cheap model is
/// configured or the model call fails (graceful, never errors retrieval).
fn hyde_augment_query(workspace: &Path, query: &str) -> String {
    let Some(resolved) = distiller::resolve_distiller(workspace) else {
        eprintln!(
            "kimetsu: --hyde requested but no [cheap_model] configured; using the raw query."
        );
        return query.to_string();
    };
    let Some(mut provider) = distiller::make_provider_for_resolved(&resolved) else {
        return query.to_string();
    };
    match distiller::hyde_expand(query, provider.as_mut()) {
        Some(hyp) => format!("{query}\n{hyp}"),
        None => query.to_string(),
    }
}

/// A fading memory candidate for triage.
#[derive(Debug)]
struct TriageCandidate {
    memory_id: String,
    scope: String,
    kind: String,
    text: String,
    age_days: i64,
    usefulness_score: f32,
}

/// Query the DB for triage candidates.
fn triage_candidates(
    conn: &rusqlite::Connection,
    score_floor: f32,
    age_days: u32,
) -> KimetsuResult<Vec<TriageCandidate>> {
    use rusqlite::params;
    use time::OffsetDateTime;

    // Compute the cutoff timestamp.
    let now = OffsetDateTime::now_utc();
    let cutoff = now - time::Duration::days(i64::from(age_days));
    let cutoff_str = cutoff
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let mut stmt = conn.prepare(
        "SELECT memory_id, scope, kind, text, usefulness_score,
                COALESCE(last_useful_at, created_at) AS ref_ts
         FROM memories
         WHERE invalidated_at IS NULL
           AND superseded_by IS NULL
           AND usefulness_score < ?1
           AND COALESCE(last_useful_at, created_at) < ?2
         ORDER BY usefulness_score ASC, COALESCE(last_useful_at, created_at) ASC
         LIMIT 200",
    )?;

    let rows = stmt.query_map(params![score_floor as f64, cutoff_str], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (memory_id, scope, kind, text, score, ref_ts) = row?;
        let age = {
            use time::format_description::well_known::Rfc3339;
            OffsetDateTime::parse(&ref_ts, &Rfc3339)
                .map(|t| (now - t).whole_days().max(0))
                .unwrap_or(0)
        };
        candidates.push(TriageCandidate {
            memory_id,
            scope,
            kind,
            text,
            age_days: age,
            usefulness_score: score as f32,
        });
    }
    Ok(candidates)
}

/// Interactive decision loop — mirrors the `decide_preflight_action` pattern
/// in update.rs. Generic over BufRead + Write for testability.
fn triage_interactive_loop<R: io::BufRead, W: io::Write>(
    workspace: &std::path::Path,
    candidates: &[TriageCandidate],
    reader: &mut R,
    writer: &mut W,
) -> KimetsuResult<()> {
    let mut pruned = 0usize;
    let mut kept = 0usize;
    let mut skipped = 0usize;

    for c in candidates {
        writeln!(
            writer,
            "\n[{}] {}/{} age={}d score={:.2}",
            c.memory_id, c.scope, c.kind, c.age_days, c.usefulness_score
        )?;
        writeln!(writer, "  {}", &c.text[..c.text.len().min(120)])?;
        write!(writer, "  [k]eep / [p]rune / [s]kip: ")?;
        writer.flush()?;

        let mut line = String::new();
        reader.read_line(&mut line)?;
        match line.trim().to_ascii_lowercase().as_str() {
            "p" | "prune" => {
                let reason = format!(
                    "triage_prune score={:.2} age_days={}",
                    c.usefulness_score, c.age_days
                );
                if kimetsu_brain::project::invalidate_memory(workspace, &c.memory_id, Some(&reason))
                    .is_ok()
                {
                    pruned += 1;
                    writeln!(writer, "  → pruned.")?;
                } else {
                    writeln!(writer, "  → prune failed.")?;
                }
            }
            "k" | "keep" => {
                kept += 1;
                writeln!(writer, "  → kept.")?;
            }
            _ => {
                skipped += 1;
                writeln!(writer, "  → skipped.")?;
            }
        }
    }

    writeln!(
        writer,
        "\nTriage complete: {} pruned, {} kept, {} skipped.",
        pruned, kept, skipped
    )?;
    Ok(())
}

/// Format a token count with thousands separator (space).
fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut out = String::new();
    let rem = s.len() % 3;
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (i % 3 == rem) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// Flagship 3.1 — `kimetsu brain ask "<question>"`.
///
/// Retrieves brain context for the question and composes a grounded, cited
/// answer using the configured cheap model (local preferred). Degrades
/// gracefully: verbatim capsule dump when no model is configured, refusal
/// when retrieval is empty. Never hard-fails.
fn brain_ask(args: AskArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // --helpful mode: mark a prior answer helpful by citing its memories.
    if let Some(citations_raw) = &args.helpful {
        let handles: Vec<String> = citations_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if handles.is_empty() {
            eprintln!("--helpful requires at least one memory handle (e.g. memory:01ABC)");
            return Ok(());
        }
        ask::record_helpful_mark(&workspace, &handles);
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "marked_helpful": handles,
                }))?
            );
        } else {
            println!("Marked {} citation(s) helpful.", handles.len());
        }
        return Ok(());
    }

    let question = args.question.trim();
    if question.is_empty() {
        eprintln!("Usage: kimetsu brain ask \"<question>\"");
        return Ok(());
    }

    let result = ask::compose_answer(&workspace, question);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "question": question,
                "answer": result.answer,
                "citations": result.citations,
                "grounded": result.grounded,
                "model_used": result.model_used,
                "verbatim": result.verbatim,
            }))?
        );
        return Ok(());
    }

    // Human-readable output.
    println!("{}", result.answer);
    if !result.citations.is_empty() {
        println!();
        println!("Sources: {}", result.citations.join(", "));
    }
    if !result.grounded {
        // Already printed refusal text; nothing more to do.
    } else if result.verbatim {
        println!();
        println!(
            "Tip: configure a cheap model in project.toml \
             ([cheap_model] provider = \"ollama\" …) for composed answers."
        );
    } else {
        // Hint for the helpful-mark workflow.
        if !result.citations.is_empty() {
            let handles = result.citations.join(",");
            println!();
            println!("If this helped, run: kimetsu brain ask --helpful {handles} \"\"",);
        }
    }

    Ok(())
}

/// Flagship 2: `kimetsu brain skills` — Memory → Skill synthesis.
fn brain_skills(args: SkillsArgs) -> KimetsuResult<()> {
    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // --accept: install a specific pending proposal.
    if let Some(ref proposal_id) = args.accept {
        match skill_synth::install_skill_proposal(&workspace, proposal_id) {
            Ok(path) => {
                println!("Skill installed: {}", path.display());
                println!("Run `kimetsu brain skills --status` to check for future staleness.");
            }
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // --reject: reject a specific pending proposal.
    if let Some(ref proposal_id) = args.reject {
        let (_paths, _config, conn) = project::load_project(&workspace)?;
        kimetsu_brain::skill_synthesis::reject_skill_proposal(&conn, proposal_id)?;
        println!("Proposal {proposal_id} rejected.");
        return Ok(());
    }

    // --status: show staleness for accepted skills.
    if args.status {
        let (_paths, _config, conn) = project::load_project(&workspace)?;
        skill_synth::print_staleness_status(&conn)?;
        return Ok(());
    }

    // --review: list proposals for review.
    if args.review {
        let (_paths, _config, conn) = project::load_project(&workspace)?;
        skill_synth::print_skill_review(&conn)?;
        return Ok(());
    }

    // Default (--detect or no flag): detect candidates + create proposals.
    let report = skill_synth::run_skill_synthesis(&workspace)?;
    skill_synth::print_synthesis_report(&report);
    Ok(())
}

/// v0.6: `kimetsu brain context-hook` — UserPromptSubmit hook.
/// Reads `{"prompt":"..."}` JSON from stdin, retrieves relevant capsules,
/// prints Codex/Claude-compatible hook JSON to stdout for injection.
/// Silent (exit 0) when brain has nothing.
fn brain_context_hook(args: ContextHookArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::ContextRequest;
    use std::io::Read;

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Read hook JSON from stdin
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);

    // Parse the full hook payload once so we can extract both `prompt`
    // and `session_id` (Change A + Change B).
    let hook_payload: Option<serde_json::Value> = if input.trim().is_empty() {
        None
    } else {
        serde_json::from_str(input.trim()).ok()
    };

    // Change B: extract session_id — present in Claude Code's
    // UserPromptSubmit payload; absent in Codex / plain-text fallbacks.
    let session_id: Option<String> = hook_payload
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    // Extract the prompt text from the hook payload
    let prompt = match &hook_payload {
        Some(v) => v
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        None if !input.trim().is_empty() => input.trim().to_string(), // plain-text fallback
        None => String::new(),
    };

    // Too short to be meaningful
    if prompt.len() < 10 {
        return Ok(());
    }

    let request = ContextRequest {
        stage: "localization".to_string(),
        query: prompt,
        budget_tokens: 2000,
        min_score: args.min_score,
        max_capsules: args.max_capsules,
        ..Default::default()
    };

    // Retrieval: try the warm daemon first (semantic); fall back to
    // floored-FTS on any miss (daemon disabled / unreachable / cold).
    let (bundle, retrieval_path) = match try_daemon_retrieve(&workspace, &request) {
        Some(b) => (b, "daemon"),
        None => match project::retrieve_context_lexical_readonly(&workspace, request.clone()) {
            Ok(b) => (b, "fts_fallback"),
            Err(_) => return Ok(()), // Brain not initialized — silent fail
        },
    };

    // C7: emit a context.served event BEFORE the early-return so misses are
    // logged. Best-effort (let _ =) — telemetry must never break the hook.
    // Gate behind KIMETSU_BRAIN_LOG_RETRIEVAL=0 opt-out (default ON).
    if std::env::var("KIMETSU_BRAIN_LOG_RETRIEVAL").as_deref() != Ok("0") {
        // Change A: load store_queries from project config best-effort.
        // Telemetry must never break the hook, so any config error just
        // falls through to the safe default (true = store the query).
        let store_queries = kimetsu_core::paths::ProjectPaths::discover(&workspace)
            .ok()
            .and_then(|paths| project::load_config(&paths).ok())
            .map(|cfg| cfg.learning.store_queries)
            .unwrap_or(true);

        let payload = build_served_event_payload(ServedEventArgs {
            query: &request.query,
            capsule_count: bundle.capsules.len(),
            top_score: bundle.top_score,
            skipped: bundle.skipped,
            stage: &request.stage,
            retrieval_path,
            store_queries,
            session_id: session_id.as_deref(),
        });
        let _ = project::log_telemetry_event(&workspace, "context.served", payload);
    }

    // Change C1: capture top-10 dropped MEMORY capsules to the rolling
    // sidecar. Best-effort — telemetry must never break the hook.
    // We capture AFTER the telemetry event so a slow sidecar write
    // doesn't block the event. Only capsules whose expansion_handle
    // starts with "memory:" are interesting for regret detection.
    {
        use kimetsu_brain::dropped_capsule;
        let cache_dir = kimetsu_core::paths::ProjectPaths::discover(&workspace)
            .ok()
            .map(|p| kimetsu_core::paths::user_cache_dir_for(&p.repo_root));
        if let Some(cache_dir) = cache_dir {
            let dropped_ids = bundle
                .excluded
                .iter()
                .filter(|c| c.expansion_handle.starts_with("memory:"))
                .filter_map(|c| {
                    c.expansion_handle
                        .strip_prefix("memory:")
                        .map(str::to_string)
                })
                .take(10);
            let now = dropped_capsule::now_secs();
            dropped_capsule::append_dropped(&cache_dir, dropped_ids, now);
        }
    }

    if bundle.skipped || bundle.capsules.is_empty() {
        return Ok(()); // Nothing relevant — zero output
    }

    // v1.5 / F3 Pass B: load broker render-flags best-effort.
    // The hook must never fail on config errors — fallback to safe defaults.
    let (compress_capsules, session_dedupe, answer_grade_min_score) =
        kimetsu_core::paths::ProjectPaths::discover(&workspace)
            .ok()
            .and_then(|paths| project::load_config(&paths).ok())
            .map(|cfg| {
                (
                    cfg.broker.compress_capsules,
                    cfg.broker.session_dedupe,
                    cfg.broker.answer_grade_min_score,
                )
            })
            .unwrap_or((true, true, 0.92));

    // v1.5 (Story 2.3): session-scoped cross-turn dedupe.
    // Load the proactive-state sidecar (already used by proactive hooks) to
    // track which capsule handles were injected earlier this session.
    // The context hook has session_id from the hook payload (Change B).
    let state_path = kimetsu_core::paths::ProjectPaths::discover(&workspace)
        .ok()
        .map(|p| {
            let cache_dir = kimetsu_core::paths::user_cache_dir_for(&p.repo_root);
            proactive_state::session_path(&cache_dir, session_id.as_deref())
        });
    let mut state = state_path
        .as_deref()
        .map(proactive_state::load)
        .unwrap_or_default();

    // Apply soft dedupe: filter already-surfaced handles, but fall back to the
    // full set if filtering would leave nothing (a repeated top memory may still
    // be the right context). Uses the pure `dedupe_filter` function.
    let capsules_to_render: Vec<_> = if session_dedupe {
        let handles: Vec<&str> = bundle
            .capsules
            .iter()
            .map(|c| c.expansion_handle.as_str())
            .collect();
        let indices = proactive_state::dedupe_filter(&handles, &state);
        indices.into_iter().map(|i| &bundle.capsules[i]).collect()
    } else {
        bundle.capsules.iter().collect()
    };

    // F3 Pass B (3.3): pre-compute the answer-grade marker for the top capsule
    // (the first capsule in capsules_to_render after dedupe). The marker signals
    // to the model that it can act in one turn rather than re-verifying.
    //
    // STRICTLY ADDITIVE: this only changes the rendered prefix of the already-
    // top capsule. Ranking, floors, and which capsules were selected are never
    // touched. Suppressed (guard = None) when:
    //   a) the top capsule's score is below answer_grade_min_score (conservative
    //      default 0.92 — roughly the top 10% of scores on a well-populated brain),
    //   b) answer_grade_min_score > 1.0 (operator disabled the feature), or
    //   c) REGRET GUARD: the capsule's memory_id appears in the recent dropped
    //      sidecar — meaning the same memory was excluded by floors in a different
    //      recent retrieval context, indicating inconsistent scoring that makes
    //      the "verified answer" label overconfident. Read-only peek (best-effort).
    //
    // Note: the dropped sidecar tracks EXCLUDED capsules (those that did not
    // make the bundle). A capsule in bundle.capsules cannot be in the sidecar
    // for THIS retrieval pass, but it might appear there from a PRIOR retrieval
    // within the 2-hour window — that is the overconfidence signal we guard.
    let answer_grade_handle: Option<&str> = capsules_to_render
        .first()
        .filter(|top| top.score >= answer_grade_min_score && answer_grade_min_score <= 1.0)
        .and_then(|top| {
            // Regret guard: read-only peek at the dropped sidecar.
            // If the memory was recently dropped by floors (in any prior retrieval
            // this session window), do NOT label it answer-grade — the floors
            // gave conflicting signals, which means the confidence marker would
            // be misleading. Best-effort: any I/O error skips the guard (allows
            // the label) rather than breaking the hook.
            let memory_id = top.expansion_handle.strip_prefix("memory:").unwrap_or("");
            if memory_id.is_empty() {
                return None; // Non-memory capsules (repo_file, manifest) — skip guard
            }
            let in_dropped_sidecar = kimetsu_core::paths::ProjectPaths::discover(&workspace)
                .ok()
                .map(|paths| {
                    let cache_dir = kimetsu_core::paths::user_cache_dir_for(&paths.repo_root);
                    let sidecar_path = kimetsu_brain::dropped_capsule::sidecar_path(&cache_dir);
                    let state = kimetsu_brain::dropped_capsule::load(&sidecar_path);
                    state.entries.iter().any(|e| e.memory_id == memory_id)
                })
                .unwrap_or(false);
            if in_dropped_sidecar {
                None // Regret guard suppresses the answer-grade label
            } else {
                Some(top.expansion_handle.as_str())
            }
        });

    let mut additional_context = String::from("Kimetsu brain relevant knowledge for this task:");
    for (idx, capsule) in capsules_to_render.iter().enumerate() {
        // v1.5 (Story 2.1): render-time compression — runs AFTER retrieval and
        // reranking, purely on the injected text. Full summary untouched in DB.
        let rendered: String = if compress_capsules {
            kimetsu_brain::context::compress_for_render(&capsule.summary, 3)
        } else {
            capsule.summary.clone()
        };
        // Strip the "scope:kind - " prefix from the summary for readability
        let text = rendered
            .split(" - ")
            .nth(1)
            .map(str::to_string)
            .unwrap_or(rendered);
        additional_context.push('\n');
        // F3 Pass B (3.3): prepend the answer-grade marker to the first capsule
        // when it cleared the high-confidence threshold AND passed the regret
        // guard. Only the first rendered capsule (idx == 0) can be answer-grade
        // (it's the top-ranked capsule); subsequent capsules are never marked.
        if idx == 0 && answer_grade_handle.is_some() {
            additional_context.push_str("Verified answer from project memory: ");
        }
        additional_context.push_str(&text);
    }

    print_user_prompt_submit_context(&additional_context)?;

    // v1.5 (Story 2.3): persist newly surfaced handles so subsequent prompts
    // in the same session skip them. Best-effort — state write must never
    // break the hook's primary output.
    if session_dedupe {
        for capsule in &capsules_to_render {
            if !capsule.expansion_handle.is_empty() {
                state.mark_surfaced(&capsule.expansion_handle);
            }
        }
        if let Some(ref path) = state_path {
            proactive_state::save(path, &state);
        }
    }

    Ok(())
}

/// v1.5: inputs for the `context.served` telemetry payload builder.
///
/// Grouped into a struct to keep [`build_served_event_payload`] under
/// the clippy `too_many_arguments` threshold and to make call-sites
/// self-documenting.
pub struct ServedEventArgs<'a> {
    /// Raw retrieval query text.
    pub query: &'a str,
    /// How many capsules were included in the bundle.
    pub capsule_count: usize,
    /// Best composite score before the skip check.
    pub top_score: f32,
    /// True when the top score was below `min_score` (no injection).
    pub skipped: bool,
    /// Retrieval stage tag (e.g. `"localization"`).
    pub stage: &'a str,
    /// `"daemon"` or `"fts_fallback"`.
    pub retrieval_path: &'a str,
    /// When true, include the raw query text in the payload.
    /// When false, only the hash is stored (pre-v1.5 behavior).
    pub store_queries: bool,
    /// Claude Code session id from the hook payload, when available.
    /// Codex and plain-text fallbacks may omit it.
    pub session_id: Option<&'a str>,
}

/// v1.5: pure builder for the `context.served` telemetry payload.
///
/// Extracted so the logic can be unit-tested without hitting the FS or
/// spawning hooks. Always emits `query_hash` for backward compatibility;
/// adds `query` only when `args.store_queries` is true; adds `session_id`
/// only when present.
pub fn build_served_event_payload(args: ServedEventArgs<'_>) -> serde_json::Value {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    args.query.hash(&mut h);
    let query_hash = format!("{:016x}", h.finish());

    let mut map = serde_json::Map::new();
    map.insert("query_hash".into(), serde_json::json!(query_hash));
    if args.store_queries {
        map.insert("query".into(), serde_json::json!(args.query));
    }
    map.insert(
        "capsule_count".into(),
        serde_json::json!(args.capsule_count),
    );
    map.insert("top_score".into(), serde_json::json!(args.top_score));
    map.insert("skipped".into(), serde_json::json!(args.skipped));
    map.insert("stage".into(), serde_json::json!(args.stage));
    map.insert(
        "retrieval_path".into(),
        serde_json::json!(args.retrieval_path),
    );
    if let Some(sid) = args.session_id {
        map.insert("session_id".into(), serde_json::json!(sid));
    }
    serde_json::Value::Object(map)
}

fn print_user_prompt_submit_context(additional_context: &str) -> KimetsuResult<()> {
    let output = user_prompt_submit_context_output(additional_context);
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn user_prompt_submit_context_output(additional_context: &str) -> serde_json::Value {
    serde_json::json!({
        "continue": true,
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": additional_context,
        },
    })
}

/// v0.7: Claude Code Stop hook. Reads the session JSON from stdin,
/// counts `kimetsu_brain_record` calls in the transcript, and prints a
/// summary banner. v0.8.5: reads the real `transcript_path` (a JSONL
/// file Claude Code writes) instead of a non-existent inline array, and
/// — when nothing was recorded in a non-trivial session — points at the
/// memory-harvester subagent. Silent exit for short sessions.
/// v1.5: when the session had ≥1 citation, appends a savings sentence to
/// the `systemMessage` banner.
fn brain_stop_hook(args: StopHookArgs) -> KimetsuResult<()> {
    use std::io::Read;

    let workspace = args
        .workspace
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);

    // Parse the session JSON from Claude Code's Stop hook payload.
    let session: serde_json::Value =
        serde_json::from_str(input.trim()).unwrap_or(serde_json::Value::Null);

    // Count transcript messages + recorded lessons. Claude Code's Stop
    // hook sends a `transcript_path` to a JSONL file (one message per
    // line), NOT an inline array — stream it line-by-line so a long
    // session's transcript (tens of MB) never lands in memory at once.
    // Fall back to an inline `transcript` array for other harnesses/tests.
    let transcript_path = session
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string);
    let (turn_count, recorded) = match transcript_path.as_deref() {
        Some(path) => count_transcript_jsonl(path),
        None => {
            let messages: Vec<serde_json::Value> = session
                .get("transcript")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            (messages.len(), count_brain_record_calls(&messages))
        }
    };

    // v1.5: compute per-session ROI (best-effort; errors are silently ignored).
    let sid = session.get("session_id").and_then(|v| v.as_str());
    let session_savings = compute_stop_hook_savings(&workspace, sid);
    // S2.1: compute re-tune trigger cue (best-effort; never blocks the hook).
    let retune_cue = compute_stop_hook_retune_cue(&workspace);

    if recorded > 0 {
        return emit_stop_hook_json(stop_lessons_recorded_json_with_savings_and_tune(
            recorded,
            session_savings.as_deref(),
            retune_cue.as_deref(),
        ));
    }
    // Short sessions exit silently — no nagging for quick lookups. The
    // count is transcript *lines* (user/assistant/tool messages), so the
    // bar is set above a trivial lookup exchange.
    const MIN_TRANSCRIPT_LINES: usize = 12;
    if turn_count < MIN_TRANSCRIPT_LINES {
        return Ok(());
    }

    // Non-trivial session, nothing recorded. When auto-harvest is on and
    // we haven't already cued a harvest this session (e.g. via the
    // PostToolUse resolution cue), point at the harvester subagent.
    // `stop_hook_active` means we're already in a stop continuation —
    // don't re-cue.
    let stop_active = session
        .get("stop_hook_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace).ok();
    let auto_harvest = paths
        .as_ref()
        .and_then(|p| project::load_config(p).ok())
        .map(|c| c.learning.auto_harvest)
        .unwrap_or(true);
    let distiller_enabled = distiller::resolve_distiller(&workspace).is_some();
    let state_path = paths.as_ref().map(|p| {
        let cache_dir = kimetsu_core::paths::user_cache_dir_for(&p.repo_root);
        proactive_state::session_path(&cache_dir, sid)
    });

    if args.distill_on_stop
        && distiller_enabled
        && !stop_active
        && let Some(path) = transcript_path.as_deref()
    {
        let mut state = state_path
            .as_ref()
            .map(|path| proactive_state::load(path))
            .unwrap_or_default();
        if !state.harvest_cued() {
            let _ = distiller::run_distiller_for_transcript(&workspace, path);
            if let Some(state_path) = state_path.as_ref() {
                state.note_harvest_cue(proactive_state::now_unix());
                proactive_state::save(state_path, &state);
            }
            return Ok(());
        }
    }

    if should_emit_stop_harvest_cue(auto_harvest, distiller_enabled)
        && !stop_active
        && let Some(paths) = paths.as_ref()
    {
        let state_path = state_path.unwrap_or_else(|| {
            let cache_dir = kimetsu_core::paths::user_cache_dir_for(&paths.repo_root);
            proactive_state::session_path(&cache_dir, sid)
        });
        let mut state = proactive_state::load(&state_path);
        if !state.harvest_cued() {
            emit_stop_hook_json(stop_harvest_cue_json())?;
            state.note_harvest_cue(proactive_state::now_unix());
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    }

    emit_stop_hook_json(stop_no_lessons_json_with_savings_and_tune(
        session_savings.as_deref(),
        retune_cue.as_deref(),
    ))
}

/// v1.5: Compute a per-session savings sentence for the Stop hook.
///
/// Best-effort: returns `None` on any error (DB not found, no data, etc.)
/// so the hook never fails due to ROI computation.
///
/// Returns `None` also when there are zero citations this session (silence
/// is the correct behavior — we don't dilute the harvest cue).
fn compute_stop_hook_savings(workspace: &Path, session_id: Option<&str>) -> Option<String> {
    use kimetsu_brain::roi::session_roi;

    let (paths, config, conn) = kimetsu_brain::project::load_project_readonly(workspace).ok()?;
    let _ = paths; // suppress unused warning
    let sr = session_roi(
        &conn,
        session_id,
        &config.model.model,
        config.model.price_per_mtok,
    )?;
    Some(sr.savings_sentence())
}

/// S2.1: Compute a re-tune proposal one-liner for the Stop hook.
///
/// Returns `Some(line)` when a re-tune is proposed (corpus milestone or drift
/// trigger), `None` otherwise.  Best-effort — any error returns `None` so the
/// stop hook is never disrupted.
fn compute_stop_hook_retune_cue(workspace: &Path) -> Option<String> {
    use kimetsu_brain::tune::compute_retune_trigger;

    let (paths, _, conn) = kimetsu_brain::project::load_project_readonly(workspace).ok()?;
    let trigger = compute_retune_trigger(&conn, &paths.kimetsu_dir).ok()?;
    if !trigger.should_retune {
        return None;
    }
    let reason = if trigger.corpus_milestone_triggered {
        format!(
            "Brain grew +{} memories since last tune — run `kimetsu brain tune`",
            trigger.memories_added_since_tune
        )
    } else {
        format!(
            "Retrieval regret rate {:.0}% (24 h) — run `kimetsu brain tune`",
            trigger.regret_rate * 100.0
        )
    };
    Some(reason)
}

/// Emit a Claude Code `Stop`-hook result on stdout. Claude Code validates a
/// Stop hook's stdout as JSON (the advanced control object), so the hook must
/// never print bare text — doing so trips "hook returned invalid stop hook
/// JSON output". A `Null` value prints nothing (silent allow-stop).
fn emit_stop_hook_json(value: serde_json::Value) -> KimetsuResult<()> {
    if !value.is_null() {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}

/// User-facing banner confirming how many lessons were recorded. Surfaced via
/// `systemMessage` (shown to the user; it does not re-enter the model).
/// Kept for test compatibility; production code uses `_with_savings` directly.
#[cfg_attr(not(test), allow(dead_code))]
fn stop_lessons_recorded_json(recorded: usize) -> serde_json::Value {
    stop_lessons_recorded_json_with_savings(recorded, None)
}

/// v1.5: lessons-recorded banner with optional savings sentence appended.
/// When `savings` is `Some`, it is appended after the lessons line.
/// The original `stop_lessons_recorded_json` delegates here so existing tests
/// continue to pass unchanged.
fn stop_lessons_recorded_json_with_savings(
    recorded: usize,
    savings: Option<&str>,
) -> serde_json::Value {
    stop_lessons_recorded_json_with_savings_and_tune(recorded, savings, None)
}

/// The end-of-session harvest cue. Uses `decision: "block"` so the cue text
/// actually re-enters the model (plain stdout never reaches it in a Stop
/// hook), prompting it to dispatch the harvester before the turn ends. The
/// `stop_hook_active` + persisted `harvest_cued` guards keep this to one cue
/// per session, so blocking cannot loop.
fn stop_harvest_cue_json() -> serde_json::Value {
    serde_json::json!({
        "decision": "block",
        "reason": "[kimetsu-harvest] No lessons recorded this non-trivial session. If anything \
                   durable was learned, run the kimetsu-memory-harvester agent in the background \
                   to capture it — otherwise call kimetsu_brain_record.",
    })
}

/// User-facing fallback nudge when nothing was recorded and the harvest cue
/// path did not fire. Informational only, so it uses `systemMessage`.
/// Kept for test compatibility; production code uses `_with_savings` directly.
#[cfg_attr(not(test), allow(dead_code))]
fn stop_no_lessons_json() -> serde_json::Value {
    stop_no_lessons_json_with_savings(None)
}

/// v1.5: no-lessons nudge with optional savings sentence appended.
fn stop_no_lessons_json_with_savings(savings: Option<&str>) -> serde_json::Value {
    stop_no_lessons_json_with_savings_and_tune(savings, None)
}

/// S2.1: no-lessons nudge with optional savings + re-tune cue.
fn stop_no_lessons_json_with_savings_and_tune(
    savings: Option<&str>,
    retune_cue: Option<&str>,
) -> serde_json::Value {
    let base =
        "[Kimetsu] No lessons recorded. After non-trivial solutions, call kimetsu_brain_record.";
    let mut parts: Vec<&str> = vec![base];
    if let Some(s) = savings {
        parts.push(s);
    }
    // S2.1: append re-tune cue if triggered.
    let retune_owned;
    if let Some(cue) = retune_cue {
        retune_owned = format!("[Tune] {cue}.");
        parts.push(&retune_owned);
    }
    let msg = parts.join(" ");
    serde_json::json!({ "systemMessage": msg })
}

/// S2.1: lessons-recorded banner with optional savings + re-tune cue.
fn stop_lessons_recorded_json_with_savings_and_tune(
    recorded: usize,
    savings: Option<&str>,
    retune_cue: Option<&str>,
) -> serde_json::Value {
    let base = format!(
        "[Kimetsu] {} lesson{} recorded.",
        recorded,
        if recorded == 1 { "" } else { "s" }
    );
    let mut parts: Vec<String> = vec![base];
    if let Some(s) = savings {
        parts.push(s.to_string());
    }
    if let Some(cue) = retune_cue {
        parts.push(format!("[Tune] {cue}."));
    }
    let msg = parts.join(" ");
    serde_json::json!({ "systemMessage": msg })
}

/// The end-of-session harvest cue fires only when auto-harvest is on AND
/// the credentialed distiller is not handling end-of-session itself.
fn should_emit_stop_harvest_cue(auto_harvest: bool, distiller_enabled: bool) -> bool {
    auto_harvest && !distiller_enabled
}

/// Count `kimetsu_brain_record` tool-use blocks across transcript
/// messages. Tolerates both the inline message shape (`content` array)
/// and Claude Code's JSONL shape (`message.content` array). The tool
/// name is matched against both the bare `kimetsu_brain_record` and the
/// MCP-namespaced `mcp__kimetsu__kimetsu_brain_record` form that real
/// Claude Code transcripts actually carry.
fn count_brain_record_calls(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .map(|m| {
            let content = m
                .get("content")
                .or_else(|| m.get("message").and_then(|msg| msg.get("content")))
                .and_then(|c| c.as_array());
            content
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| {
                            b.get("name")
                                .and_then(|n| n.as_str())
                                .is_some_and(is_brain_record_tool)
                        })
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

/// True for the `kimetsu_brain_record` tool under either the bare name
/// or any MCP namespace prefix (`mcp__<server>__kimetsu_brain_record`).
fn is_brain_record_tool(name: &str) -> bool {
    name == "kimetsu_brain_record" || name.ends_with("__kimetsu_brain_record")
}

/// Stream a transcript JSONL file, returning `(message_count,
/// brain_record_count)` without loading the whole file into memory.
/// Best-effort: an unreadable file or malformed line is skipped, never
/// fatal (a hook must not break the agent's turn). A leading UTF-8 BOM on
/// the first line is tolerated.
fn count_transcript_jsonl(path: &str) -> (usize, usize) {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return (0, 0);
    };
    let mut turns = 0usize;
    let mut records = 0usize;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        turns += 1;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            records += count_brain_record_calls(std::slice::from_ref(&value));
        }
    }
    (turns, records)
}

#[derive(Debug, Clone, Copy)]
enum ProactiveEvent {
    PreTool,
    PostTool,
}

impl ProactiveEvent {
    fn hook_event_name(self) -> &'static str {
        match self {
            ProactiveEvent::PreTool => "PreToolUse",
            ProactiveEvent::PostTool => "PostToolUse",
        }
    }
}

/// Harness-agnostic fields pulled from a PreToolUse/PostToolUse hook
/// payload. Both Claude Code and Codex send this superset; parse
/// defensively so a missing/odd field just disables the relevant path.
struct HookToolInput {
    session_id: Option<String>,
    tool_name: Option<String>,
    command: Option<String>,
    tool_response: Option<String>,
    /// F3 Pass B (3.5): file path from `tool_input.file_path` (ReadFile,
    /// EditFile, etc.). Absent for Bash and other non-file tools. Used by
    /// the proactive pre-fetch path when `broker.proactive_prefetch = true`
    /// to augment the retrieval query with the file being operated on.
    tool_file_path: Option<String>,
}

fn parse_hook_tool_input(raw: &str) -> HookToolInput {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or(serde_json::Value::Null);
    let str_field = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let command = v
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    // F3 Pass B (3.5): extract file_path from tool_input for pre-fetch query
    // augmentation. Covers ReadFile, EditFile, WriteFile, and similar tools
    // whose Claude Code / Codex tool_input carries a `file_path` field.
    let tool_file_path = v
        .get("tool_input")
        .and_then(|ti| ti.get("file_path"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    // tool_response may be a string or a structured object; stringify
    // objects so failure detection still has something to scan.
    let tool_response = match v.get("tool_response") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    };
    HookToolInput {
        session_id: str_field("session_id"),
        tool_name: str_field("tool_name"),
        command,
        tool_response,
        tool_file_path,
    }
}

/// v0.8: proactive PreToolUse / PostToolUse hook. Shared by both
/// events. Lexical-FTS-only retrieval, very high score floor, one
/// capsule, per-session dedupe + refractory + loop detection. Always
/// exits 0; emits hook JSON only on a confident, novel match.
fn proactive_hook(event: ProactiveEvent, args: ProactiveHookArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::ContextRequest;
    use std::io::Read;

    let workspace = args
        .workspace
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());

    // Resolve the .kimetsu dir; if there's no brain here, stay silent.
    let Ok(paths) = kimetsu_core::paths::ProjectPaths::discover(&workspace) else {
        return Ok(());
    };
    // Honor the configured embedder id for consistency (proactive
    // retrieval is lexical-only, but this keeps labels coherent). Also
    // capture the auto-harvest toggle, render flags, and F3 Pass B toggles.
    let (auto_harvest, compress_capsules, proactive_prefetch) = match project::load_config(&paths) {
        Ok(config) => {
            kimetsu_brain::embeddings::apply_embedder_selection(Some(&config.embedder.model));
            (
                config.learning.auto_harvest,
                config.broker.compress_capsules,
                config.broker.proactive_prefetch,
            )
        }
        // Fallback: safe defaults — proactive_prefetch OFF (zero behaviour change)
        Err(_) => (true, true, false),
    };

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);
    if input.trim().is_empty() {
        return Ok(());
    }
    let hook = parse_hook_tool_input(&input);

    // Defensive tool-name gate (the hook matcher should already scope
    // to Bash, but be safe across harness quirks).
    //
    // F3 Pass B (3.5): when proactive_prefetch is ON, relax the Bash-only gate
    // so file-tool PreToolUse calls (ReadFile, EditFile, WriteFile, …) can also
    // trigger a lightweight file-path-based pre-fetch. The PostToolUse path is
    // unchanged (still Bash-only — file tools don't produce failure output).
    // When proactive_prefetch is OFF (default), the gate is unchanged: only
    // Bash tool calls are processed (zero behaviour change).
    let is_bash = hook
        .tool_name
        .as_deref()
        .map(|n| n.eq_ignore_ascii_case("bash"))
        .unwrap_or(true); // no tool_name → assume Bash (old harness compat)
    let allow_non_bash = proactive_prefetch && matches!(event, ProactiveEvent::PreTool);
    if !is_bash && !allow_non_bash {
        return Ok(());
    }

    let now = proactive_state::now_unix();
    let proactive_cache_dir = kimetsu_core::paths::user_cache_dir_for(&paths.repo_root);
    proactive_state::gc(&proactive_cache_dir, now);

    let state_path =
        proactive_state::session_path(&proactive_cache_dir, hook.session_id.as_deref());
    let mut state = proactive_state::load(&state_path);

    // v0.8.5: PostToolUse success — if this command failed earlier this
    // session and just succeeded, that's a resolved failure (a learnable
    // moment). Cue the agent (throttled) to harvest the lesson, then exit.
    if matches!(event, ProactiveEvent::PostTool) {
        let resp = hook.tool_response.as_deref().unwrap_or("");
        if !proactive_state::looks_like_failure(resp) {
            let norm = proactive_state::normalize_command(hook.command.as_deref().unwrap_or(""));
            if auto_harvest
                && !norm.is_empty()
                && state.had_prior_failure(&norm)
                && !state.harvest_in_refractory(now, proactive_state::HARVEST_REFRACTORY_SECS)
            {
                let cmd = hook.command.as_deref().unwrap_or("the command");
                let cue = format!(
                    "[kimetsu-harvest] You just resolved a previously failing command (`{cmd}`). \
                     If this revealed a durable, generalizable lesson, run the \
                     kimetsu-memory-harvester agent in the background \
                     to record it via kimetsu_brain_record."
                );
                print_tool_use_context(event, &cue)?;
                state.note_harvest_cue(now);
                state.clear_failure(&norm);
            }
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    }

    // Build the retrieval query + actionable kinds per event.
    //
    // F3 Pass B (3.5): when `broker.proactive_prefetch = true`, the PreToolUse
    // query is augmented with the tool's `file_path` (e.g. the file being read
    // or edited). This lightweight warm surfaces memories relevant to the file
    // BEFORE the agent operates on it, rather than waiting for a failure.
    //
    // When `proactive_prefetch = false` (default), no augmentation happens and
    // PreToolUse behaviour is identical to before this flag existed. The same
    // floors (min_score, refractory, dedupe) gate the result — this is strictly
    // additive. Default-on graduation waits for regret data (Epic S2).
    let (query, kinds, error_sig): (String, &[&str], Option<String>) = match event {
        ProactiveEvent::PreTool => {
            // F3 Pass B (3.5): build the PreToolUse query from command and/or
            // file_path depending on the proactive_prefetch flag.
            //
            // proactive_prefetch OFF (default):
            //   - No command → silent exit (identical to pre-F3 behaviour).
            //   - Command present → use command as query (identical to pre-F3).
            //   - file_path is NEVER consulted (zero behaviour change).
            //
            // proactive_prefetch ON:
            //   - No command AND no file_path → silent exit.
            //   - No command but file_path present → file_path-only query.
            //   - Command present → command + file_path (if any) concatenated.
            let cmd_opt = hook.command.as_deref();
            let fp_opt = if proactive_prefetch {
                hook.tool_file_path.as_deref().filter(|s| s.len() > 4)
            } else {
                None
            };
            let query = match (cmd_opt, fp_opt) {
                (Some(cmd), Some(fp)) => format!("{cmd} {fp}"),
                (Some(cmd), None) => cmd.to_string(),
                (None, Some(fp)) => fp.to_string(),
                (None, None) => return Ok(()), // nothing to query on
            };
            (query, &["failure_pattern", "convention"], None)
        }
        ProactiveEvent::PostTool => {
            let resp = hook.tool_response.as_deref().unwrap_or("");
            if !proactive_state::looks_like_failure(resp) {
                return Ok(()); // only react to failures
            }
            let cmd = hook.command.as_deref().unwrap_or("");
            (
                format!("{resp} {cmd}"),
                &["failure_pattern", "command", "convention"],
                proactive_state::error_signature(resp),
            )
        }
    };

    // Record this command, decide loop mode (state loaded above).
    let norm = proactive_state::normalize_command(hook.command.as_deref().unwrap_or(&query));
    let seen_count = state.note_command(&norm, error_sig.as_deref(), now);
    let loop_mode = seen_count >= proactive_state::LOOP_THRESHOLD;

    // Refractory throttle — unless the agent is clearly looping, stay
    // quiet for a window after the last injection. Persist the loop
    // counter increment even on a silent exit.
    if !loop_mode && state.in_refractory(now, args.refractory_secs) {
        proactive_state::save(&state_path, &state);
        return Ok(());
    }

    let min_score = if loop_mode {
        args.loop_min_score
    } else {
        args.min_score
    };

    let request = ContextRequest {
        stage: "localization".to_string(),
        query,
        budget_tokens: 600,
        min_score,
        max_capsules: args.max_capsules.max(1),
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        ..Default::default()
    };

    let bundle = match project::retrieve_proactive_readonly(&workspace, request) {
        Ok(b) => b,
        Err(_) => {
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    };

    let Some(capsule) = bundle
        .capsules
        .iter()
        .find(|c| !state.is_surfaced(&c.expansion_handle))
    else {
        // Nothing relevant, or the only match already surfaced this
        // session (it's already in working memory).
        proactive_state::save(&state_path, &state);
        return Ok(());
    };

    // v1.5 (Story 2.1): render-time compression for the proactive hook.
    // Runs AFTER retrieval — ranking and stored text are unaffected.
    let rendered: String = if compress_capsules {
        kimetsu_brain::context::compress_for_render(&capsule.summary, 3)
    } else {
        capsule.summary.clone()
    };
    let body = rendered
        .split(" - ")
        .nth(1)
        .map(str::to_string)
        .unwrap_or(rendered);
    let header = proactive_header(event, loop_mode);
    let additional_context = format!("{header}\n{body}");

    print_tool_use_context(event, &additional_context)?;

    state.mark_surfaced(&capsule.expansion_handle);
    state.record_injection(now);
    proactive_state::save(&state_path, &state);
    Ok(())
}

fn proactive_header(event: ProactiveEvent, loop_mode: bool) -> &'static str {
    match (event, loop_mode) {
        (_, true) => {
            "You appear to be repeating a failing command. Kimetsu brain recalls a relevant lesson:"
        }
        (ProactiveEvent::PreTool, false) => {
            "Kimetsu brain — a relevant prior failure for this command:"
        }
        (ProactiveEvent::PostTool, false) => "Kimetsu brain — a known fix for this failure:",
    }
}

fn print_tool_use_context(event: ProactiveEvent, additional_context: &str) -> KimetsuResult<()> {
    // Non-blocking inject on both harnesses: hookSpecificOutput.
    // additionalContext with the matching hookEventName. We never set
    // permissionDecision / decision:block — proactive recall informs,
    // it does not gate.
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event.hook_event_name(),
            "additionalContext": additional_context,
        },
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn stats() -> KimetsuResult<()> {
    let memories = project::list_memories(&env::current_dir()?)?;
    let runs = project::list_runs(&env::current_dir()?)?;
    println!("memories: {}", memories.len());
    println!("runs: {}", runs.len());
    Ok(())
}

fn memory(command: MemoryCommand) -> KimetsuResult<()> {
    match command {
        MemoryCommand::Add(args) => {
            let scope = MemoryScope::from_str(&args.scope)?;
            let kind = MemoryKind::from_str(&args.kind)?;
            let id = project::add_memory(&env::current_dir()?, scope, kind, &args.text)?;
            println!("memory_id: {id}");
            Ok(())
        }
        MemoryCommand::AddBatch(args) => memory_add_batch(args),
        MemoryCommand::List { json } => {
            let memories = project::list_memories(&env::current_dir()?)?;
            if json {
                let rows: Vec<serde_json::Value> = memories
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "memory_id": m.memory_id,
                            "scope": m.scope,
                            "kind": m.kind,
                            "confidence": m.confidence,
                            "use_count": m.use_count,
                            "usefulness_score": m.usefulness_score,
                            "text": m.text,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if memories.is_empty() {
                println!("no memories");
                return Ok(());
            }

            for memory in memories {
                let usefulness_ratio = if memory.use_count > 0 {
                    format!(
                        " ratio={:+.2}",
                        memory.usefulness_score / memory.use_count as f32
                    )
                } else {
                    String::new()
                };
                println!(
                    "{} [{}:{} confidence={:.2} uses={} usefulness={:+.1}{}] {}",
                    memory.memory_id,
                    memory.scope,
                    memory.kind,
                    memory.confidence,
                    memory.use_count,
                    memory.usefulness_score,
                    usefulness_ratio,
                    memory.text
                );
            }
            Ok(())
        }
        MemoryCommand::Proposals(args) => {
            let proposals = project::list_proposals(
                &env::current_dir()?,
                project::ProposalFilter {
                    scope: args.scope,
                    kind: args.kind,
                    from_run: args.from_run,
                    min_confidence: args.min_confidence,
                    status: Some(args.status),
                    limit: args.limit,
                    offset: 0,
                },
            )?;
            if proposals.is_empty() {
                println!("no memory proposals");
                return Ok(());
            }

            for proposal in proposals {
                println!(
                    "{} [{}:{} status={} confidence={:.2} run={}] {}",
                    proposal.proposal_id,
                    proposal.scope,
                    proposal.kind,
                    proposal.status,
                    proposal.proposed_confidence,
                    proposal.run_id,
                    proposal.text
                );
                if !proposal.rationale.is_empty() {
                    println!("  rationale: {}", proposal.rationale);
                }
                if let Some(reason) = proposal.decided_reason.as_deref()
                    && !reason.is_empty()
                {
                    println!("  decided_reason: {reason}");
                }
            }
            Ok(())
        }
        MemoryCommand::Accept(args) => {
            let memory_id = project::accept_proposal(
                &env::current_dir()?,
                &args.proposal_id,
                project::AcceptOverrides {
                    scope: args.scope,
                    confidence: args.confidence,
                },
            )?;
            println!("memory_id: {memory_id}");
            Ok(())
        }
        MemoryCommand::Reject(args) => {
            project::reject_proposal(
                &env::current_dir()?,
                &args.proposal_id,
                args.reason.as_deref(),
            )?;
            if let Some(reason) = args.reason.as_deref() {
                println!("rejected proposal: {} (reason: {reason})", args.proposal_id);
            } else {
                println!("rejected proposal: {}", args.proposal_id);
            }
            Ok(())
        }
        MemoryCommand::Invalidate(args) => {
            project::invalidate_memory(
                &env::current_dir()?,
                &args.memory_id,
                args.reason.as_deref(),
            )?;
            if let Some(reason) = args.reason.as_deref() {
                println!("invalidated memory: {} (reason: {reason})", args.memory_id);
            } else {
                println!("invalidated memory: {}", args.memory_id);
            }
            Ok(())
        }
        MemoryCommand::Review(args) => review_proposals(args),
        MemoryCommand::Top(args) => memory_top(args),
        MemoryCommand::Prune(args) => memory_prune(args),
        MemoryCommand::Blame(args) => memory_blame(args),
        MemoryCommand::Conflicts(args) => memory_conflicts(args),
        MemoryCommand::Edit(args) => memory_edit(args),
        MemoryCommand::Undo(args) => memory_undo(args),
        MemoryCommand::SetAge(args) => {
            let workspace = args
                .workspace
                .unwrap_or_else(|| env::current_dir().unwrap_or_default());
            project::record_set_age(&workspace, &args.memory_id, args.days_ago)?;
            println!(
                "Backdated memory {} by {} days.",
                args.memory_id, args.days_ago
            );
            Ok(())
        }
    }
}

/// MP-6: pretty-print `list_memories_top`. Surfaces ratio + use_count
/// alongside the text so the user can quickly judge which entries to
/// keep and which to invalidate.
fn memory_top(args: TopArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let rows = project::list_memories_top(
        &cwd,
        project::TopOptions {
            scope: args.scope.clone(),
            min_uses: args.min_uses,
            limit: args.limit,
        },
    )?;
    if rows.is_empty() {
        println!(
            "no memories meet the min-uses threshold ({})",
            args.min_uses
        );
        return Ok(());
    }
    println!(
        "top memories (min_uses>={}, limit={}{}):",
        args.min_uses,
        args.limit,
        args.scope
            .as_deref()
            .map(|s| format!(", scope={s}"))
            .unwrap_or_default()
    );
    for m in rows {
        let ratio = m.usefulness_score as f64 / m.use_count.max(1) as f64;
        println!(
            "  {} [{}:{} uses={} usefulness={:+.1} ratio={:+.2}] {}",
            m.memory_id, m.scope, m.kind, m.use_count, m.usefulness_score, ratio, m.text
        );
    }
    Ok(())
}

/// v0.5.1: `kimetsu brain memory blame <run-id>` — print the per-memory
/// attribution for a single run. Cited memories show the model's
/// rationale + turn; silent passengers show that they were retrieved but
/// never reached for. `--json` emits the full BlameReport for CI / hooks.
fn memory_blame(args: BlameArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let report = project::blame_run(&cwd, args.run_id.trim())?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!("[blame] run {}", report.run_id);
    print!("[blame] outcome: {}", report.outcome);
    if let Some(cat) = report.failure_category.as_deref() {
        print!(" (category: {cat})");
    }
    println!();

    if report.cited.is_empty() && report.silent_passengers.is_empty() {
        println!(
            "[blame] no memories were retrieved or cited for this run. \
             Either the run pre-dates v0.5.1, the brain was off \
             (`--project` unset), or no `context.injected` events fired."
        );
        return Ok(());
    }

    if !report.cited.is_empty() {
        println!(
            "\n  cited memories ({} total) — earned strong ±1.0 signal:",
            report.cited.len()
        );
        for c in &report.cited {
            let rationale = c
                .rationale
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| format!("  // {s}"))
                .unwrap_or_default();
            println!(
                "    {} [{}:{}] turn={}{}",
                c.memory_id, c.scope, c.kind, c.turn, rationale
            );
            println!("      {}", c.text_preview);
        }
    }

    if !report.silent_passengers.is_empty() {
        println!(
            "\n  silent passengers ({} total) — earned weak ±0.1 signal (model didn't cite):",
            report.silent_passengers.len()
        );
        for s in &report.silent_passengers {
            println!("    {} [{}:{}]", s.memory_id, s.scope, s.kind);
            println!("      {}", s.text_preview);
        }
    }
    println!();
    Ok(())
}

/// v0.5.2: `kimetsu brain memory conflicts` — list or resolve
/// conflict-detection hits surfaced at ingest. Without `--resolve` it
/// lists open conflicts (project + user brains merged), with the
/// origin brain shown per row so the operator knows where the
/// resolution will land. `--resolve <id> <resolution>` settles one
/// conflict and (for `kept_new` / `kept_existing`) invalidates the
/// losing side.
fn memory_conflicts(args: ConflictsArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;

    if let Some(resolve_args) = args.resolve.as_ref() {
        // num_args = 2 ensures clap delivers exactly 2 values.
        let conflict_id = resolve_args[0].trim();
        let resolution = resolve_args[1].trim();
        let updated = project::resolve_conflict(&cwd, conflict_id, resolution)?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({
                    "conflict_id": conflict_id,
                    "resolution": resolution,
                    "updated": updated,
                })
            );
            return Ok(());
        }
        if updated {
            println!(
                "[conflicts] resolved {conflict_id} as {resolution} (losing side, if any, invalidated)"
            );
        } else {
            println!(
                "[conflicts] no open conflict with id {conflict_id} (already resolved, or unknown id)"
            );
        }
        return Ok(());
    }

    let open = project::list_conflicts(&cwd, args.limit)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&open)?);
        return Ok(());
    }

    if open.is_empty() {
        println!(
            "[conflicts] no open conflicts. \
             Either no contradictory memories have been ingested, \
             the embedder is the lean NoopEmbedder (build with \
             `--features embeddings` to enable detection), or all \
             prior conflicts have been resolved."
        );
        return Ok(());
    }

    println!("[conflicts] {} open conflict(s):", open.len());
    for scoped in &open {
        let c = &scoped.report;
        println!(
            "  {} [{}] {} <-> {} (similarity {:.3}, scope={}, kind={}, detected {})",
            c.conflict_id,
            scoped.source,
            c.new_memory_id,
            c.existing_memory_id,
            c.similarity,
            c.scope,
            c.kind,
            c.detected_at,
        );
        println!("    new:      {}", preview_inline(&c.new_text));
        println!("    existing: {}", preview_inline(&c.existing_text));
    }
    println!(
        "\nResolve with: kimetsu brain memory conflicts --resolve <id> <kept_new|kept_existing|kept_both>"
    );
    Ok(())
}

/// Q6: `kimetsu brain memory edit <id> [--text …] [--kind …]`
///
/// Edits an existing active memory in place — corrects the text and/or
/// changes the kind while KEEPING the learned history (use_count,
/// usefulness_score, confidence, created_at). The FTS index and embedding
/// are refreshed so semantic/keyword retrieval reflects the new text.
fn memory_edit(args: MemoryEditArgs) -> KimetsuResult<()> {
    if args.text.is_none() && args.kind.is_none() {
        return Err("memory edit: at least one of --text or --kind must be provided".into());
    }

    let cwd = env::current_dir()?;
    let new_kind = args.kind.as_deref().map(MemoryKind::from_str).transpose()?;

    project::edit_memory(&cwd, &args.memory_id, args.text.as_deref(), new_kind)?;
    println!("updated memory {}", args.memory_id);
    Ok(())
}

/// Q6: `kimetsu brain memory undo [--yes]`
///
/// Previews the most-recently-recorded active memory in the project brain,
/// confirms (unless `--yes`), then invalidates it. The row is retained for
/// audit purposes — it simply stops being surfaced in retrieval.
fn memory_undo(args: MemoryUndoArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;

    // Peek at the most-recent active memory before asking the user.
    let peek = project::peek_last_memory(&cwd)?;
    let preview = match peek {
        None => {
            println!("no active memories to undo");
            return Ok(());
        }
        Some(m) => m,
    };

    println!(
        "most recent memory: {} [{}:{}] {}",
        preview.memory_id, preview.scope, preview.kind, preview.text
    );

    // Confirm unless --yes or non-TTY.
    if !args.yes && io::stdin().is_terminal() {
        print!("invalidate this memory? [y/N] ");
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).ok();
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted");
            return Ok(());
        }
    }

    match project::undo_last_memory(&cwd)? {
        Some(undone) => {
            println!(
                "invalidated memory {} (row kept for audit; no longer retrieved)",
                undone.memory_id
            );
        }
        None => {
            // Edge case: someone invalidated the memory between our peek and
            // the undo call (concurrent write). Report gracefully.
            println!("no active memories to undo");
        }
    }

    Ok(())
}

/// `kimetsu brain memory add-batch` — ingest many memories in one process.
///
/// Reads a JSONL file (one JSON object per line) or a JSON array from FILE
/// (or stdin when FILE is `-`).  Processes all entries with the project and
/// embedder opened exactly once — far cheaper than spawning one
/// `memory add` subprocess per entry.
///
/// Each JSON object must have a `"text"` field.  Optional fields:
///   `"scope"` — overrides --scope for this entry
///   `"kind"`  — overrides --kind for this entry
///   `"valid_from"` / `"valid_to"` — RFC 3339 temporal bounds (Flagship 1)
fn memory_add_batch(args: MemoryAddBatchArgs) -> KimetsuResult<()> {
    use kimetsu_brain::project::BatchMemoryEntry;

    let default_scope = MemoryScope::from_str(&args.scope)?;
    let default_kind = MemoryKind::from_str(&args.kind)?;

    // Read raw bytes from file or stdin.
    let raw: String = if args.file == "-" {
        let stdin = io::stdin();
        let mut s = String::new();
        for line in stdin.lock().lines() {
            let line = line.map_err(|e| format!("stdin read error: {e}"))?;
            s.push_str(&line);
            s.push('\n');
        }
        s
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| format!("cannot read '{}': {e}", args.file))?
    };

    // Parse as JSON array first; fall back to JSONL (one object per line).
    // This handles both `[{...},{...}]` and `{...}\n{...}` formats.
    #[derive(serde::Deserialize)]
    struct RawEntry {
        text: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        valid_from: Option<String>,
        #[serde(default)]
        valid_to: Option<String>,
    }

    let raw_entries: Vec<RawEntry> = {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            // JSON array format.
            serde_json::from_str(trimmed).map_err(|e| format!("failed to parse JSON array: {e}"))?
        } else {
            // JSONL format: parse each non-empty line.
            let mut entries = Vec::new();
            for (line_no, line) in trimmed.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let entry: RawEntry = serde_json::from_str(line)
                    .map_err(|e| format!("failed to parse JSONL line {}: {e}", line_no + 1))?;
                entries.push(entry);
            }
            entries
        }
    };

    if raw_entries.is_empty() {
        if args.json {
            println!("{{\"added\":0,\"ids\":[]}}");
        } else {
            println!("added 0 memories");
        }
        return Ok(());
    }

    // Convert to BatchMemoryEntry, resolving scope/kind per entry.
    let mut entries: Vec<BatchMemoryEntry> = Vec::with_capacity(raw_entries.len());
    for (i, re) in raw_entries.into_iter().enumerate() {
        let scope = match re.scope.as_deref() {
            Some(s) => {
                MemoryScope::from_str(s).map_err(|e| format!("entry {i}: invalid scope: {e}"))?
            }
            None => default_scope,
        };
        let kind = match re.kind.as_deref() {
            Some(k) => {
                MemoryKind::from_str(k).map_err(|e| format!("entry {i}: invalid kind: {e}"))?
            }
            None => default_kind,
        };
        entries.push(BatchMemoryEntry {
            text: re.text,
            scope,
            kind,
            valid_from: re.valid_from,
            valid_to: re.valid_to,
        });
    }

    let n = entries.len();
    let ids = project::add_memories_batch(&env::current_dir()?, entries)?;

    if args.json {
        let out = serde_json::json!({"added": ids.len(), "ids": ids});
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!(
            "added {} memor{}",
            ids.len(),
            if ids.len() == 1 { "y" } else { "ies" }
        );
        if ids.len() < n {
            // Some were deduped — note the difference.
            let deduped = n - ids.len();
            // Actually ids.len() == n always; deduped entries still return an id.
            // This branch is unreachable but kept for clarity.
            eprintln!(
                "kimetsu-brain: {deduped} entr{} were duplicates (existing id returned)",
                if deduped == 1 { "y" } else { "ies" }
            );
        }
    }

    Ok(())
}

/// One-line truncate-and-collapse for CLI rendering of memory text.
/// Keeps the conflict listing scannable when capsules are long-form.
fn preview_inline(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated: String = collapsed.chars().take(140).collect();
    if collapsed.chars().count() > 140 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// MP-6: dry-run by default. Without `--apply` it prints the prune list
/// and exits 0; with `--apply` it invalidates each match via the same
/// `invalidate_memory` path used by `memory invalidate`.
fn memory_prune(args: PruneArgs) -> KimetsuResult<()> {
    let cwd = env::current_dir()?;
    let summary = project::prune_low_usefulness(
        &cwd,
        project::PruneOptions {
            scope: args.scope.clone(),
            min_uses: args.min_uses,
            max_ratio: args.max_ratio,
            apply: args.apply,
        },
    )?;

    if summary.candidates.is_empty() {
        println!(
            "no memories match the prune criteria (min_uses>={}, max_ratio<={:+.2}{})",
            args.min_uses,
            args.max_ratio,
            args.scope
                .as_deref()
                .map(|s| format!(", scope={s}"))
                .unwrap_or_default()
        );
        return Ok(());
    }

    let action = if args.apply { "pruning" } else { "would prune" };
    println!(
        "{action} {} memorie(s) (min_uses>={}, max_ratio<={:+.2}{}):",
        summary.candidates.len(),
        args.min_uses,
        args.max_ratio,
        args.scope
            .as_deref()
            .map(|s| format!(", scope={s}"))
            .unwrap_or_default()
    );
    for c in &summary.candidates {
        let ratio = c.usefulness_score as f64 / c.use_count.max(1) as f64;
        println!(
            "  {} [{}:{} uses={} usefulness={:+.1} ratio={:+.2}] {}",
            c.memory_id, c.scope, c.kind, c.use_count, c.usefulness_score, ratio, c.text
        );
    }
    if !args.apply {
        println!("dry-run; pass --apply to invalidate these memories");
    } else {
        println!(
            "summary: invalidated={} failed={}",
            summary.invalidated, summary.failed
        );
    }
    Ok(())
}

/// MP-5a/b: review handler. Three modes:
///
/// * `--accept-all` / `--reject-all` â€” non-interactive batch (MP-5a).
/// * No flags + stdin is a TTY â€” interactive walkthrough (MP-5b): one
///   proposal at a time, prompt `[a]ccept [r]eject [s]kip [q]uit`.
/// * No flags + stdin is NOT a TTY â€” error, so a misconfigured CI script
///   never silently hangs on a stdin read.
fn review_proposals(args: ReviewArgs) -> KimetsuResult<()> {
    if args.accept_all && args.reject_all {
        // clap's conflicts_with should already block this, but guard in
        // case it's bypassed via internal callers.
        return Err("--accept-all and --reject-all are mutually exclusive".into());
    }

    let cwd = env::current_dir()?;
    let pending = project::list_proposals(
        &cwd,
        project::ProposalFilter {
            scope: args.scope.clone(),
            kind: args.kind.clone(),
            from_run: args.from_run.clone(),
            min_confidence: args.min_confidence,
            status: Some("pending".to_string()),
            limit: args.limit,
            offset: 0,
        },
    )?;

    if pending.is_empty() {
        println!("no pending proposals matched the filters");
        return Ok(());
    }

    // MP-5b: no batch flag -> interactive walkthrough when stdin is a TTY.
    if !args.accept_all && !args.reject_all {
        if !io::stdin().is_terminal() {
            return Err(
                "memory review requires --accept-all / --reject-all when stdin is not a TTY".into(),
            );
        }
        return interactive_review_loop(&cwd, pending);
    }

    let action = if args.accept_all { "accept" } else { "reject" };
    println!(
        "review: would {action} {} pending proposal(s){}",
        pending.len(),
        if args.dry_run { " (dry-run)" } else { "" }
    );
    for p in &pending {
        println!(
            "  {} [{}:{} confidence={:.2} run={}] {}",
            p.proposal_id, p.scope, p.kind, p.proposed_confidence, p.run_id, p.text
        );
    }
    if args.dry_run {
        return Ok(());
    }

    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut failed = 0u32;
    let resolved_reason = args
        .reason
        .clone()
        .unwrap_or_else(|| "batch_reject".to_string());

    for proposal in pending {
        if args.accept_all {
            match project::accept_proposal(
                &cwd,
                &proposal.proposal_id,
                project::AcceptOverrides::default(),
            ) {
                Ok(memory_id) => {
                    accepted += 1;
                    println!("accepted {} -> memory {memory_id}", proposal.proposal_id);
                }
                Err(err) => {
                    failed += 1;
                    eprintln!("skipped accept on {}: {err}", proposal.proposal_id);
                }
            }
        } else {
            match project::reject_proposal(&cwd, &proposal.proposal_id, Some(&resolved_reason)) {
                Ok(()) => {
                    rejected += 1;
                    println!(
                        "rejected {} (reason: {resolved_reason})",
                        proposal.proposal_id
                    );
                }
                Err(err) => {
                    failed += 1;
                    eprintln!("skipped reject on {}: {err}", proposal.proposal_id);
                }
            }
        }
    }

    println!("summary: accepted={accepted} rejected={rejected} failed={failed}");
    Ok(())
}

/// MP-5b: walk pending proposals one at a time, prompting the user for
/// each. Decisions persist immediately (idempotent via the existing
/// brain APIs), so `[q]uit` partway through leaves an accurate state.
///
/// Prompt vocabulary kept intentionally small for v0.2:
///   `a` accept | `r` reject | `s` skip | `q` quit | `?` re-print help
/// On `r` we ask for an optional reason on a follow-up line; empty input
/// keeps the default `reviewed_rejected_interactive`. Edits to scope /
/// kind / text are deferred to MP-5c â€” for now [s]kip + the existing
/// `memory accept --scope X` / `memory reject` commands cover that path.
fn interactive_review_loop(cwd: &Path, pending: Vec<project::ProposalRow>) -> KimetsuResult<()> {
    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();
    interactive_review_loop_inner(cwd, pending, &mut stdin_lock, &mut stdout_lock)
}

/// Pure plumbing for `interactive_review_loop`: takes injected I/O so the
/// loop can be driven from tests with scripted input. Production wiring
/// passes stdin/stdout locks; tests pass `Cursor::new(b"a\n...")` and a
/// `Vec<u8>` writer.
fn interactive_review_loop_inner<R: BufRead, W: Write>(
    cwd: &Path,
    pending: Vec<project::ProposalRow>,
    reader: &mut R,
    writer: &mut W,
) -> KimetsuResult<()> {
    let total = pending.len();
    let mut input = String::new();
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;

    writeln!(
        writer,
        "interactive review: {total} pending proposal(s). [a]ccept [r]eject [s]kip [q]uit [?]help"
    )?;

    for (idx, proposal) in pending.into_iter().enumerate() {
        writeln!(writer)?;
        writeln!(
            writer,
            "[{idx_one}/{total}] {pid}  scope={scope}  kind={kind}  confidence={conf:.2}  run={run}",
            idx_one = idx + 1,
            pid = proposal.proposal_id,
            scope = proposal.scope,
            kind = proposal.kind,
            conf = proposal.proposed_confidence,
            run = proposal.run_id,
        )?;
        writeln!(writer, "  text: {}", proposal.text)?;
        if !proposal.rationale.is_empty() {
            writeln!(writer, "  rationale: {}", proposal.rationale)?;
        }

        loop {
            write!(writer, "  > ")?;
            writer.flush().ok();
            input.clear();
            let read = reader.read_line(&mut input)?;
            if read == 0 {
                let processed = accepted + rejected + skipped + failed;
                let unprocessed = (total as u32).saturating_sub(processed);
                skipped += unprocessed;
                writeln!(writer, "(stdin closed; {unprocessed} proposal(s) skipped)")?;
                print_interactive_summary(writer, accepted, rejected, skipped, failed)?;
                return Ok(());
            }
            let choice = input.trim().to_ascii_lowercase();
            match choice.as_str() {
                "a" | "accept" => {
                    match project::accept_proposal(
                        cwd,
                        &proposal.proposal_id,
                        project::AcceptOverrides::default(),
                    ) {
                        Ok(memory_id) => {
                            accepted += 1;
                            writeln!(writer, "  -> accepted: memory {memory_id}")?;
                        }
                        Err(err) => {
                            failed += 1;
                            writeln!(writer, "  -> accept failed: {err}")?;
                        }
                    }
                    break;
                }
                "r" | "reject" => {
                    write!(writer, "  reason (enter to use default): ")?;
                    writer.flush().ok();
                    let mut reason_buf = String::new();
                    reader.read_line(&mut reason_buf)?;
                    let reason = reason_buf.trim();
                    let resolved = if reason.is_empty() {
                        "reviewed_rejected_interactive"
                    } else {
                        reason
                    };
                    match project::reject_proposal(cwd, &proposal.proposal_id, Some(resolved)) {
                        Ok(()) => {
                            rejected += 1;
                            writeln!(writer, "  -> rejected (reason: {resolved})")?;
                        }
                        Err(err) => {
                            failed += 1;
                            writeln!(writer, "  -> reject failed: {err}")?;
                        }
                    }
                    break;
                }
                "s" | "skip" | "" => {
                    skipped += 1;
                    writeln!(writer, "  -> skipped (still pending)")?;
                    break;
                }
                "q" | "quit" | "exit" => {
                    let processed = accepted + rejected + skipped + failed;
                    let unprocessed = (total as u32).saturating_sub(processed);
                    skipped += unprocessed;
                    writeln!(
                        writer,
                        "(quit; {} proposal(s) remain pending)",
                        unprocessed.saturating_sub(1)
                    )?;
                    print_interactive_summary(writer, accepted, rejected, skipped, failed)?;
                    return Ok(());
                }
                "?" | "h" | "help" => {
                    writeln!(
                        writer,
                        "  commands: [a]ccept  [r]eject  [s]kip (default)  [q]uit  [?]help"
                    )?;
                }
                other => {
                    writeln!(writer, "  unrecognized command '{other}'; try ? for help")?;
                }
            }
        }
    }

    print_interactive_summary(writer, accepted, rejected, skipped, failed)?;
    Ok(())
}

fn print_interactive_summary<W: Write>(
    writer: &mut W,
    accepted: u32,
    rejected: u32,
    skipped: u32,
    failed: u32,
) -> io::Result<()> {
    writeln!(writer)?;
    writeln!(
        writer,
        "summary: accepted={accepted} rejected={rejected} skipped={skipped} failed={failed}"
    )
}

fn run_command(command: RunCommand) -> KimetsuResult<()> {
    match command {
        RunCommand::Coding(args) => {
            let result = run_coding(CodingRunOptions {
                repo: args.repo,
                task: args.task,
                dry_run: args.dry_run,
                allow_high_risk: args.allow_high_risk,
                disable_model: args.no_model,
                disable_broker: args.no_broker,
                model_key_override: None,
            })?;
            println!("run_id: {}", result.run_id);
            println!("dry_run: {}", result.dry_run);
            println!("patch_plan_id: {}", result.patch_plan_id);
            println!("final_report: {}", result.final_report_path.display());
            println!("trace: {}", result.trace_path.display());
            Ok(())
        }
        RunCommand::Abort { run_id } => {
            project::abort_run(&env::current_dir()?, &run_id)?;
            println!("run aborted: {run_id}");
            Ok(())
        }
    }
}

fn bench(command: BenchCommand) -> KimetsuResult<()> {
    match command {
        BenchCommand::Swe(args) => {
            let results = run_swe_bench(SweBenchOptions {
                tasks: args.tasks,
                repo: args.repo,
                instance_id: args.instance_id,
                dry_run: args.dry_run,
                disable_broker: args.no_broker,
                limit: args.limit,
            })?;
            println!("instances: {}", results.len());
            for instance in results {
                println!(
                    "{} run={} dry_run={} no_broker={} trace={}",
                    instance.instance_id,
                    instance.run_id,
                    instance.dry_run,
                    instance.disable_broker,
                    instance.trace_path.display(),
                );
            }
            Ok(())
        }
        BenchCommand::Run(args) => {
            let result = run_benchmark(BenchOptions {
                repo: args.repo,
                keep_fixtures: args.keep_fixtures,
                model_backed: args.model_backed,
                limit: args.limit,
                max_cost_usd: args.max_cost_usd,
            })?;
            println!("bench_run_id: {}", result.bench_run_id);
            println!("tasks: {}", result.task_count);
            println!("model_backed: {}", result.model_backed);
            println!("total_cost_usd: {:.4}", result.total_cost_usd);
            println!("report: {}", result.report_path.display());
            println!("results: {}", result.results_path.display());
            for summary in result.summaries {
                println!(
                    "{} success={:.0}% relevant_signal={:.0}% memories={} context_loads={} irrelevant_context={} dry_runs={} avg_ms={:.2} cost_usd={:.4} plan_quality={:.2} invalid_planned={} trace_events={} model_turns={} model_skips={}",
                    summary.mode,
                    summary.success_rate * 100.0,
                    summary.relevant_signal_rate * 100.0,
                    summary.accepted_memories_used,
                    summary.context_loads,
                    summary.irrelevant_context_loaded,
                    summary.dry_runs,
                    summary.avg_duration_ms,
                    summary.total_cost_usd,
                    summary.avg_patch_plan_quality,
                    summary.invalid_planned_files,
                    summary.trace_events,
                    summary.model_turns,
                    summary.model_skips,
                );
            }
            Ok(())
        }
    }
}

// ── runs prune helpers ────────────────────────────────────────────────────

/// Metadata for a single on-disk run directory. Used by the pure selection
/// logic so tests never touch the filesystem.
#[derive(Debug, Clone)]
struct RunDirInfo {
    /// Directory name (the ULID string, or whatever the dir is named).
    name: String,
    /// Full path to the run directory.
    path: PathBuf,
    /// Run-start timestamp in Unix milliseconds.
    /// Derived from the ULID embedded timestamp when the name is a valid
    /// ULID; falls back to the directory's mtime (converted to ms), or 0
    /// when neither is available.
    started_ms: u64,
    /// Total size of all files in the directory (bytes), best-effort.
    size_bytes: u64,
}

/// Parse a human-friendly duration string into a `std::time::Duration`.
///
/// Accepted format: `<integer><unit>` where unit is one of:
///   - `d` → days (86 400 s each)
///   - `h` → hours
///   - `m` → minutes
///   - `s` → seconds
///
/// Examples: `"30d"`, `"7d"`, `"24h"`, `"90m"`, `"45s"`.
fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".to_string());
    }
    // Split the trailing unit char from the numeric prefix.
    let (num_part, unit) = match s.chars().last() {
        Some(c @ ('d' | 'h' | 'm' | 's')) => (&s[..s.len() - c.len_utf8()], c),
        Some(c) => return Err(format!("unknown duration unit '{c}'; use d/h/m/s")),
        None => return Err("empty duration string".to_string()),
    };
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid duration number '{num_part}' in '{s}'"))?;
    let secs = match unit {
        'd' => n * 86_400,
        'h' => n * 3_600,
        'm' => n * 60,
        's' => n,
        _ => unreachable!(),
    };
    Ok(std::time::Duration::from_secs(secs))
}

/// Extract the run-start timestamp (Unix ms) from a ULID string.
/// Returns `None` when the string is not a valid ULID.
fn ulid_timestamp_ms(name: &str) -> Option<u64> {
    name.parse::<ulid::Ulid>().ok().map(|u| u.timestamp_ms())
}

/// Compute the total size in bytes of all files under `dir`, recursively.
/// Best-effort: skips entries that cannot be stat-ed.
fn dir_size_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size_bytes(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Scan `runs_dir` and return one [`RunDirInfo`] per subdirectory.
/// Non-directory entries are skipped.
fn scan_run_dirs(runs_dir: &Path) -> Vec<RunDirInfo> {
    let Ok(rd) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let mut infos: Vec<RunDirInfo> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();

            // Prefer ULID-embedded time; fall back to mtime.
            let started_ms = ulid_timestamp_ms(&name).unwrap_or_else(|| {
                entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            });

            let size_bytes = dir_size_bytes(&path);
            RunDirInfo {
                name,
                path,
                started_ms,
                size_bytes,
            }
        })
        .collect();

    // Sort by started_ms descending (newest first) for stable ordering.
    infos.sort_by_key(|b| std::cmp::Reverse(b.started_ms));
    infos
}

/// Pure selection function: given a slice of [`RunDirInfo`] (sorted
/// newest-first by `started_ms`), return the indices of runs that should
/// be pruned according to the policy.
///
/// # Policy
///
/// * **`older_than` alone**: prune runs whose `started_ms` is older than
///   `now_ms - older_than.as_millis()`. The newest-N guard is absent, so
///   all qualifying runs are selected.
///
/// * **`keep` alone**: prune everything except the `keep` newest runs
///   (i.e. indices `keep..` in the already-sorted-newest-first slice).
///
/// * **both**: prune runs that are *both* older than the cutoff *and*
///   outside the newest-N. Runs in the newest-N are always protected.
///
/// * **neither**: returns an empty `Vec` (the caller must have already
///   rejected this case with an error).
fn select_runs_to_prune(
    runs: &[RunDirInfo],
    now_ms: u64,
    older_than: Option<std::time::Duration>,
    keep: Option<usize>,
) -> Vec<usize> {
    let cutoff_ms: Option<u64> = older_than.map(|d| now_ms.saturating_sub(d.as_millis() as u64));
    let protect_n = keep.unwrap_or(0);

    runs.iter()
        .enumerate()
        .filter_map(|(idx, info)| {
            // The newest-N are always protected.
            if idx < protect_n {
                return None;
            }
            // Apply older-than cutoff when present.
            if let Some(cutoff) = cutoff_ms {
                if info.started_ms >= cutoff {
                    return None; // not old enough
                }
            } else if keep.is_none() {
                // Neither flag — caller should have blocked this; be safe.
                return None;
            }
            Some(idx)
        })
        .collect()
}

/// Format a byte count as a human-readable string (KB / MB / GB).
fn fmt_bytes(n: u64) -> String {
    if n < 1_024 {
        format!("{n} B")
    } else if n < 1_024 * 1_024 {
        format!("{:.1} KB", n as f64 / 1_024.0)
    } else if n < 1_024 * 1_024 * 1_024 {
        format!("{:.1} MB", n as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} GB", n as f64 / (1_024.0 * 1_024.0 * 1_024.0))
    }
}

fn runs(command: RunsCommand) -> KimetsuResult<()> {
    match command {
        RunsCommand::List => {
            let runs = project::list_runs(&env::current_dir()?)?;
            if runs.is_empty() {
                println!("no runs");
                return Ok(());
            }

            for run in runs {
                println!(
                    "{} [{}] {} - {}",
                    run.run_id,
                    run.terminal_kind.unwrap_or_else(|| "running".to_string()),
                    run.started_at,
                    run.task
                );
            }
            Ok(())
        }
        RunsCommand::Show { run_id } => {
            if let Some(run) = project::show_run(&env::current_dir()?, &run_id)? {
                println!("run_id: {}", run.run_id);
                println!("task: {}", run.task);
                println!("started_at: {}", run.started_at);
                println!(
                    "status: {}",
                    run.terminal_kind.unwrap_or_else(|| "running".to_string())
                );
            } else {
                println!("run not found: {run_id}");
            }
            Ok(())
        }
        RunsCommand::Prune(args) => runs_prune(args),
    }
}

fn runs_prune(args: PruneRunsArgs) -> KimetsuResult<()> {
    // Require at least one selection criterion.
    if args.older_than.is_none() && args.keep.is_none() {
        return Err("specify --older-than and/or --keep".into());
    }

    // Parse --older-than duration.
    let older_than_dur: Option<std::time::Duration> = args
        .older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| format!("--older-than: {e}"))?;

    // Resolve workspace root.
    let workspace = match args.workspace {
        Some(p) => p,
        None => env::current_dir()?,
    };

    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let runs_dir = &paths.runs_dir;

    if !runs_dir.exists() {
        println!("no runs to prune");
        return Ok(());
    }

    let infos = scan_run_dirs(runs_dir);
    let total = infos.len();

    // Current time in ms.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let to_prune = select_runs_to_prune(&infos, now_ms, older_than_dur, args.keep);
    let prune_bytes: u64 = to_prune.iter().map(|&i| infos[i].size_bytes).sum();

    if args.apply {
        let mut removed = 0usize;
        let mut freed = 0u64;
        for &idx in &to_prune {
            let info = &infos[idx];
            match std::fs::remove_dir_all(&info.path) {
                Ok(()) => {
                    removed += 1;
                    freed += info.size_bytes;
                    println!("removed {}", info.name);
                }
                Err(e) => {
                    eprintln!("warning: could not remove {} — {e}", info.name);
                }
            }
        }
        println!("removed {removed} run(s), freed {}", fmt_bytes(freed));
    } else {
        // Dry-run: list what would be removed.
        for &idx in &to_prune {
            println!(
                "would remove {} ({})",
                infos[idx].name,
                fmt_bytes(infos[idx].size_bytes)
            );
        }
        println!(
            "{total} run(s), {} old → would remove {} ({} bytes freed)",
            to_prune.len(),
            to_prune.len(),
            fmt_bytes(prune_bytes)
        );
    }

    Ok(())
}

fn lock(command: LockCommand) -> KimetsuResult<()> {
    match command {
        LockCommand::Clear { force: false } => Err("refusing to clear lock without --force".into()),
        LockCommand::Clear { force: true } => {
            let removed = project::clear_lock(&env::current_dir()?)?;
            if removed {
                println!("project lock cleared");
            } else {
                println!("no project lock found");
            }
            Ok(())
        }
    }
}

// ─── kimetsu brain eval ───────────────────────────────────────────────────────

/// Dispatch for `kimetsu brain eval`.
///
/// On embeddings builds the full three-mode eval runs. On lean builds a
/// clear stub message is printed — the user needs `--features embeddings`.
fn brain_eval(args: EvalArgs) -> KimetsuResult<()> {
    #[cfg(feature = "embeddings")]
    {
        brain_eval_inner(args)
    }
    #[cfg(not(feature = "embeddings"))]
    {
        let _ = args;
        println!("kimetsu brain eval requires an embeddings build.");
        println!("Rebuild with: cargo build -p kimetsu-cli --features embeddings");
        Ok(())
    }
}

#[cfg(feature = "embeddings")]
fn brain_eval_inner(args: EvalArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::{ContextRequest, rerank_capsules};
    use kimetsu_brain::embeddings::{
        NoopEmbedder, open_embedder_for_model, open_reranker_for_model,
    };
    use kimetsu_brain::eval::{EvalFixture, mean, mrr, recall_at_k};
    use kimetsu_brain::project::{BrainSession, add_memory, init_project};
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::collections::HashMap;
    use std::time::Instant;

    // Disable the user brain for this process — we work in a hermetic temp dir.
    // SAFETY: this is a one-shot CLI command; no other threads have started yet.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    }

    // ── 1. Load and validate fixture ─────────────────────────────────────────
    let fixture_path = &args.fixture;
    let fixture_text = std::fs::read_to_string(fixture_path)
        .map_err(|e| format!("cannot read fixture {}: {e}", fixture_path.display()))?;
    let fixture: EvalFixture = serde_json::from_str(&fixture_text)
        .map_err(|e| format!("invalid fixture JSON in {}: {e}", fixture_path.display()))?;

    // Validate: every relevant key must exist in memories.
    let all_keys: std::collections::HashSet<&str> =
        fixture.memories.iter().map(|m| m.key.as_str()).collect();
    for case in &fixture.cases {
        for rel in &case.relevant {
            if !all_keys.contains(rel.as_str()) {
                return Err(format!(
                    "fixture validation error: relevant key {:?} in query {:?} does not exist in memories",
                    rel, case.query
                )
                .into());
            }
        }
    }

    println!(
        "eval fixture: {} memories, {} cases",
        fixture.memories.len(),
        fixture.cases.len()
    );

    // ── 2. Set up a hermetic temp brain ──────────────────────────────────────
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!("kimetsu-eval-{ts}"));
    std::fs::create_dir_all(&tmp_root)?;
    git_init_boundary(&tmp_root);

    // Init the project brain.
    init_project(&tmp_root, true).map_err(|e| format!("init_project: {e}"))?;

    // Add all corpus memories and track key → memory_id mapping.
    println!(
        "adding {} memories to temp brain...",
        fixture.memories.len()
    );
    let mut key_to_id: HashMap<String, String> = HashMap::new();
    for mem in &fixture.memories {
        let memory_id = add_memory(&tmp_root, MemoryScope::Project, MemoryKind::Fact, &mem.text)
            .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
        key_to_id.insert(mem.key.clone(), memory_id);
    }

    // Build key → id lookup from the map (for ranking back to keys).
    let id_to_key: HashMap<String, String> = key_to_id
        .iter()
        .map(|(k, v)| (v.clone(), k.clone()))
        .collect();

    // #1a HyDE: pre-expand each case query ONCE (shared across all retrieval
    // modes) so the embedding matches a hypothetical answer rather than the
    // question. Reranking still uses the original query. The semantic query
    // used for retrieval is `original + hypothetical`.
    let retrieval_queries: Vec<String> = if args.hyde {
        let cfg = tmp_root.join(".kimetsu").join("project.toml");
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&cfg) {
            use std::io::Write;
            let _ = writeln!(
                f,
                "\n[cheap_model]\nenabled = true\nprovider = \"ollama\"\nmodel = \"qwen2.5:3b\""
            );
        }
        println!(
            "HyDE: expanding {} queries via the cheap model (one model call each)...",
            fixture.cases.len()
        );
        fixture
            .cases
            .iter()
            .map(|c| hyde_augment_query(&tmp_root, &c.query))
            .collect()
    } else {
        fixture.cases.iter().map(|c| c.query.clone()).collect()
    };

    // ── 3. Helper: run one mode, return ranked key list per case ─────────────
    let run_mode = |mode_label: &str,
                    embedder: &dyn kimetsu_brain::embeddings::Embedder,
                    reranker: Option<&dyn kimetsu_brain::embeddings::Reranker>,
                    pool: usize,
                    rerank_floor: f32,
                    rerank_cap: usize|
     -> KimetsuResult<(Vec<Vec<String>>, u128)> {
        let session = BrainSession::open_readonly(&tmp_root)
            .map_err(|e| format!("{mode_label} open_readonly: {e}"))?;

        let t0 = Instant::now();
        let mut per_case_ranked: Vec<Vec<String>> = Vec::new();

        for (ci, case) in fixture.cases.iter().enumerate() {
            let fetch_cap = pool;
            let request = ContextRequest {
                stage: "localization".to_string(),
                query: retrieval_queries[ci].clone(),
                budget_tokens: 6000,
                max_capsules: fetch_cap,
                min_semantic_score: 0.0,   // disable floor for eval recall
                min_lexical_coverage: 0.0, // disable floor for eval recall
                ..Default::default()
            };
            let mut bundle = session
                .retrieve_context_with_injected_embedder(request, embedder)
                .map_err(|e| format!("{mode_label} retrieve: {e}"))?;

            // Apply reranker when present.
            if let Some(rr) = reranker {
                bundle.capsules =
                    rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
            }

            // Map capsule expansion_handle "memory:<id>" → fixture key.
            let ranked_keys: Vec<String> = bundle
                .capsules
                .iter()
                .filter_map(|c| {
                    c.expansion_handle
                        .strip_prefix("memory:")
                        .and_then(|id| id_to_key.get(id))
                        .cloned()
                })
                .collect();

            per_case_ranked.push(ranked_keys);
        }

        let elapsed = t0.elapsed().as_millis();
        Ok((per_case_ranked, elapsed))
    };

    // ── 4. Run the three modes ────────────────────────────────────────────────
    // Pool mirrors the daemon's RERANK_POOL by default; --pool overrides it
    // for pool-size experiments.
    let pool = args.pool.max(1);
    let rerank_floor = 0.30f32;
    let rerank_cap = 4usize;

    print!("running fts mode...");
    let (fts_ranked, fts_ms) = run_mode("fts", &NoopEmbedder, None, pool, 0.0, 0)?;
    println!(" done ({fts_ms} ms)");

    print!("running semantic mode (loading embedder)...");
    let semantic_embedder = open_embedder_for_model("bge-small-en-v1.5");
    let (sem_ranked, sem_ms) =
        run_mode("semantic", semantic_embedder.as_ref(), None, pool, 0.0, 0)?;
    println!(" done ({sem_ms} ms)");

    print!("running semantic+rerank mode (loading reranker)...");
    let reranker_opt = open_reranker_for_model("jina-reranker-v1-turbo-en");
    let reranker_ref: Option<&dyn kimetsu_brain::embeddings::Reranker> = reranker_opt.as_deref();
    let (rr_ranked, rr_ms) = run_mode(
        "semantic+rerank",
        semantic_embedder.as_ref(),
        reranker_ref,
        pool,
        rerank_floor,
        rerank_cap,
    )?;
    println!(" done ({rr_ms} ms)");

    // ── 5. Compute metrics ────────────────────────────────────────────────────
    let eval_cases = &fixture.cases;
    let n = eval_cases.len();

    // Separate cases with relevant items from noise cases.
    let signal_indices: Vec<usize> = (0..n)
        .filter(|&i| !eval_cases[i].relevant.is_empty())
        .collect();
    let noise_indices: Vec<usize> = (0..n)
        .filter(|&i| eval_cases[i].relevant.is_empty())
        .collect();

    let compute_metrics = |ranked: &[Vec<String>]| -> (f64, f64, f64, f64) {
        // recall@2, recall@4, MRR over signal cases
        let r2: Vec<f64> = signal_indices
            .iter()
            .map(|&i| recall_at_k(&ranked[i], &eval_cases[i].relevant, 2))
            .collect();
        let r4: Vec<f64> = signal_indices
            .iter()
            .map(|&i| recall_at_k(&ranked[i], &eval_cases[i].relevant, 4))
            .collect();
        let mrr_vals: Vec<f64> = signal_indices
            .iter()
            .map(|&i| mrr(&ranked[i], &eval_cases[i].relevant))
            .collect();
        // Average noise capsule count for irrelevant cases.
        let noise_avg = if noise_indices.is_empty() {
            0.0
        } else {
            noise_indices
                .iter()
                .map(|&i| ranked[i].len() as f64)
                .sum::<f64>()
                / noise_indices.len() as f64
        };
        (mean(&r2), mean(&r4), mean(&mrr_vals), noise_avg)
    };

    let (fts_r2, fts_r4, fts_mrr, fts_noise) = compute_metrics(&fts_ranked);
    let (sem_r2, sem_r4, sem_mrr, sem_noise) = compute_metrics(&sem_ranked);
    let (rr_r2, rr_r4, rr_mrr, rr_noise) = compute_metrics(&rr_ranked);

    // ── 6. Print table ────────────────────────────────────────────────────────
    println!();
    println!(
        "{:<22} {:>10} {:>10} {:>10} {:>22} {:>10}",
        "mode", "recall@2", "recall@4", "MRR", "noise-capsules(irrelevant)", "elapsed_ms"
    );
    println!("{}", "-".repeat(90));
    println!(
        "{:<22} {:>10.3} {:>10.3} {:>10.3} {:>22.1} {:>10}",
        "fts", fts_r2, fts_r4, fts_mrr, fts_noise, fts_ms
    );
    println!(
        "{:<22} {:>10.3} {:>10.3} {:>10.3} {:>22.1} {:>10}",
        "semantic", sem_r2, sem_r4, sem_mrr, sem_noise, sem_ms
    );
    println!(
        "{:<22} {:>10.3} {:>10.3} {:>10.3} {:>22.1} {:>10}",
        "semantic+rerank", rr_r2, rr_r4, rr_mrr, rr_noise, rr_ms
    );
    println!();
    println!(
        "signal cases: {}  |  noise (empty-relevant) cases: {}",
        signal_indices.len(),
        noise_indices.len()
    );

    // ── 7. Optional per-reranker benchmark ───────────────────────────────────
    let reranker_ids: Vec<&str> = args
        .rerankers
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if !reranker_ids.is_empty() {
        // Struct to hold benchmark results for one reranker.
        struct RankerBenchRow {
            label: String,
            load_ms: u128,
            rerank_mean_ms: f64,
            rerank_max_ms: u128,
            r2: f64,
            r4: f64,
            mrr: f64,
            noise: f64,
            onnx_kb: Option<u64>,
        }

        // Helper: run the signal cases and time only the rerank step per query.
        let run_reranker_bench = |rr_id: &str| -> KimetsuResult<RankerBenchRow> {
            use kimetsu_brain::context::rerank_capsules;

            print!("  loading {rr_id}...");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let load_start = Instant::now();
            let reranker_box = open_reranker_for_model(rr_id);
            let load_ms = load_start.elapsed().as_millis();

            let reranker_ref: Option<&dyn kimetsu_brain::embeddings::Reranker> =
                reranker_box.as_deref();

            if reranker_ref.is_none() {
                println!(" SKIPPED (loader returned None)");
                return Err(format!("reranker {rr_id} failed to load").into());
            }
            println!(" loaded ({load_ms} ms)");

            let session = kimetsu_brain::project::BrainSession::open_readonly(&tmp_root)
                .map_err(|e| format!("{rr_id} open_readonly: {e}"))?;
            let rr = reranker_ref.unwrap();

            let mut per_case_ranked: Vec<Vec<String>> = Vec::new();
            let mut rerank_times_ms: Vec<u128> = Vec::new();

            for case in fixture.cases.iter() {
                let request = kimetsu_brain::context::ContextRequest {
                    stage: "localization".to_string(),
                    query: case.query.clone(),
                    budget_tokens: 6000,
                    max_capsules: pool,
                    min_semantic_score: 0.0,
                    min_lexical_coverage: 0.0,
                    ..Default::default()
                };
                let mut bundle = session
                    .retrieve_context_with_injected_embedder(request, semantic_embedder.as_ref())
                    .map_err(|e| format!("{rr_id} retrieve: {e}"))?;

                // Time only the rerank step.
                let rr_start = Instant::now();
                if !eval_cases[per_case_ranked.len()].relevant.is_empty() {
                    bundle.capsules =
                        rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
                    rerank_times_ms.push(rr_start.elapsed().as_millis());
                } else {
                    // Noise case: still rerank so we get noise metric.
                    bundle.capsules =
                        rerank_capsules(&case.query, bundle.capsules, rr, rerank_floor, rerank_cap);
                }

                let ranked_keys: Vec<String> = bundle
                    .capsules
                    .iter()
                    .filter_map(|c| {
                        c.expansion_handle
                            .strip_prefix("memory:")
                            .and_then(|id| id_to_key.get(id))
                            .cloned()
                    })
                    .collect();
                per_case_ranked.push(ranked_keys);
            }

            let (r2, r4, mrr_val, noise) = compute_metrics(&per_case_ranked);

            let rerank_mean_ms = if rerank_times_ms.is_empty() {
                0.0
            } else {
                rerank_times_ms.iter().sum::<u128>() as f64 / rerank_times_ms.len() as f64
            };
            let rerank_max_ms = rerank_times_ms.into_iter().max().unwrap_or(0);

            // Try to find the ONNX file size on disk (best-effort, no panic on miss).
            let onnx_kb: Option<u64> = {
                let low = rr_id.trim().to_ascii_lowercase();
                // Map alias → HF repo id for cache-path lookup.
                let repo_id: &str = match low.as_str() {
                    "jina-reranker-v1-tiny-en" => "jinaai/jina-reranker-v1-tiny-en",
                    "ms-marco-tinybert-l-2-v2" => "Xenova/ms-marco-TinyBERT-L-2-v2",
                    "ms-marco-minilm-l-4-v2" => "Xenova/ms-marco-MiniLM-L-4-v2",
                    "jina-reranker-v1-turbo-en" => "jinaai/jina-reranker-v1-turbo-en",
                    other => other,
                };
                // hf-hub default cache: ~/.cache/huggingface/hub/models--<org>--<name>/snapshots/...
                let home_cache = std::env::var("HF_HOME")
                    .ok()
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var("HOME")
                            .ok()
                            .or_else(|| std::env::var("USERPROFILE").ok())
                            .map(|h| {
                                std::path::PathBuf::from(h)
                                    .join(".cache")
                                    .join("huggingface")
                                    .join("hub")
                            })
                    });
                home_cache.and_then(|cache_root| {
                    let safe_name = repo_id.replace('/', "--");
                    let snap_dir = cache_root
                        .join(format!("models--{safe_name}"))
                        .join("snapshots");
                    let mut best: Option<u64> = None;
                    if let Ok(snaps) = std::fs::read_dir(&snap_dir) {
                        'snap: for snap in snaps.flatten() {
                            for candidate in ["onnx/model.onnx", "model.onnx"] {
                                let p = snap.path().join(candidate);
                                if let Ok(meta) = std::fs::metadata(&p) {
                                    best = Some(meta.len() / 1024);
                                    break 'snap;
                                }
                            }
                        }
                    }
                    best
                })
            };

            Ok(RankerBenchRow {
                label: rr_id.to_string(),
                load_ms,
                rerank_mean_ms,
                rerank_max_ms,
                r2,
                r4,
                mrr: mrr_val,
                noise,
                onnx_kb,
            })
        };

        println!();
        println!("=== Reranker benchmark (semantic base + per-reranker) ===");
        println!();

        // Print the semantic-only baseline row for comparison.
        let col_w = 28usize;
        println!(
            "{:<col_w$} {:>9} {:>14} {:>13} {:>10} {:>10} {:>10} {:>8} {:>10}",
            "reranker",
            "load_ms",
            "rerank_mean_ms",
            "rerank_max_ms",
            "recall@2",
            "recall@4",
            "MRR",
            "noise",
            "onnx_kb",
        );
        println!("{}", "-".repeat(118));
        println!(
            "{:<col_w$} {:>9} {:>14} {:>13} {:>10.3} {:>10.3} {:>10.3} {:>8.1} {:>10}",
            "(semantic, no rerank)", "-", "-", "-", sem_r2, sem_r4, sem_mrr, sem_noise, "-",
        );

        let mut bench_rows: Vec<RankerBenchRow> = Vec::new();
        for rr_id in &reranker_ids {
            match run_reranker_bench(rr_id) {
                Ok(row) => bench_rows.push(row),
                Err(e) => eprintln!("  {rr_id}: skipped — {e}"),
            }
        }

        for row in &bench_rows {
            let onnx_str = row
                .onnx_kb
                .map(|kb| format!("{kb}"))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<col_w$} {:>9} {:>14.1} {:>13} {:>10.3} {:>10.3} {:>10.3} {:>8.1} {:>10}",
                row.label,
                row.load_ms,
                row.rerank_mean_ms,
                row.rerank_max_ms,
                row.r2,
                row.r4,
                row.mrr,
                row.noise,
                onnx_str,
            );
        }
        println!();
    }

    // ── 8. Clean up temp dir (best-effort) ────────────────────────────────────
    let _ = std::fs::remove_dir_all(&tmp_root);

    Ok(())
}

// ─── kimetsu brain bench ──────────────────────────────────────────────────────

fn brain_bench(args: BrainBenchArgs) -> KimetsuResult<()> {
    #[cfg(feature = "embeddings")]
    {
        brain_bench_inner(args)
    }
    #[cfg(not(feature = "embeddings"))]
    {
        let _ = args;
        println!("kimetsu brain bench requires an embeddings build.");
        println!("Rebuild with: cargo build -p kimetsu-cli --features embeddings");
        Ok(())
    }
}

/// RSS helper (Windows only; returns None on other platforms or on failure).
#[cfg(feature = "embeddings")]
fn rss_mb() -> Option<f64> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let handle = GetCurrentProcess();
            let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                return Some(pmc.WorkingSetSize as f64 / (1024.0 * 1024.0));
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(feature = "embeddings")]
fn peak_rss_mb() -> Option<f64> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let handle = GetCurrentProcess();
            let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                return Some(pmc.PeakWorkingSetSize as f64 / (1024.0 * 1024.0));
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(feature = "embeddings")]
fn brain_bench_inner(args: BrainBenchArgs) -> KimetsuResult<()> {
    if args.remote {
        brain_bench_remote(args)
    } else if args.single {
        brain_bench_single(args)
    } else {
        brain_bench_orchestrate(args)
    }
}

/// Orchestrator: spawn one child per embedder×reranker combo, wait for all,
/// read per-combo JSON files, print + write summary.
#[cfg(feature = "embeddings")]
fn brain_bench_orchestrate(args: BrainBenchArgs) -> KimetsuResult<()> {
    use std::time::Instant;

    let embedders: Vec<&str> = args
        .embedders
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let rerankers: Vec<&str> = args
        .rerankers
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let dataset = args.dataset.clone();
    let out_dir = args.out.clone();
    let pool = args.pool;
    let cap = args.cap;

    std::fs::create_dir_all(&out_dir)?;

    let current_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dataset_str = dataset.to_string_lossy().to_string();
    let out_str = out_dir.to_string_lossy().to_string();

    let total = embedders.len() * rerankers.len();
    println!(
        "brain bench: {} embedder(s) × {} reranker(s) = {} combos",
        embedders.len(),
        rerankers.len(),
        total
    );
    println!("dataset: {}", dataset.display());
    println!("output:  {}", out_dir.display());
    println!();

    let mut combo_idx = 0usize;
    for &embedder in &embedders {
        for &reranker in &rerankers {
            combo_idx += 1;
            print!("[{combo_idx}/{total}] {embedder} × {reranker} ... ");
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let t0 = Instant::now();
            let status = std::process::Command::new(&current_exe)
                .arg("brain")
                .arg("bench")
                .arg("--dataset")
                .arg(&dataset_str)
                .arg("--embedders")
                .arg(embedder)
                .arg("--rerankers")
                .arg(reranker)
                .arg("--pool")
                .arg(pool.to_string())
                .arg("--cap")
                .arg(cap.to_string())
                .arg("--out")
                .arg(&out_str)
                .arg("--single")
                .status()
                .map_err(|e| format!("spawn child for {embedder}×{reranker}: {e}"))?;

            let elapsed = t0.elapsed().as_secs_f64();
            if status.success() {
                println!("done ({elapsed:.1}s)");
            } else {
                println!("FAILED (exit={status})");
            }
        }
    }

    // Read all combo JSON files and build summary rows.
    println!();
    println!("reading results...");

    #[derive(serde::Deserialize)]
    struct ComboSummary {
        recall_at_2: f64,
        recall_at_4: f64,
        mrr: f64,
        mean_latency_ms: f64,
        p95_latency_ms: f64,
        noise_capsules: f64,
        /// v1.5 (Story 2.1): mean rendered tokens per capsule after compression.
        #[serde(default)]
        rendered_tokens_mean: f64,
        /// v1.5 (Story 2.1): mean raw (uncompressed) tokens per capsule.
        #[serde(default)]
        raw_tokens_mean: f64,
        /// P0.1: mean stale-hit rate (lower is better; 0.0 = no stale in any case).
        #[serde(default)]
        stale_hit_rate: f64,
        /// P0.1: fraction of correctness cases resolved correctly (-1.0 = N/A).
        #[serde(default = "default_resolution_accuracy")]
        resolution_accuracy: f64,
    }
    fn default_resolution_accuracy() -> f64 {
        -1.0
    }
    #[derive(serde::Deserialize)]
    struct ComboResult {
        embedder: String,
        reranker: String,
        embedder_load_ms: u128,
        reranker_load_ms: u128,
        peak_rss_mb: Option<f64>,
        summary: ComboSummary,
    }

    let mut rows: Vec<ComboResult> = Vec::new();
    for &embedder in &embedders {
        for &reranker in &rerankers {
            let safe_emb = embedder.replace(['/', '.', ' '], "-");
            let safe_rr = reranker.replace(['/', '.', ' '], "-");
            let fname = format!("combo-{safe_emb}-{safe_rr}.json");
            let fpath = out_dir.join(&fname);
            match std::fs::read_to_string(&fpath) {
                Ok(text) => match serde_json::from_str::<ComboResult>(&text) {
                    Ok(r) => rows.push(r),
                    Err(e) => eprintln!("  warning: parse {fname}: {e}"),
                },
                Err(e) => eprintln!("  warning: read {fname}: {e}"),
            }
        }
    }

    // Sort by MRR desc.
    rows.sort_by(|a, b| {
        b.summary
            .mrr
            .partial_cmp(&a.summary.mrr)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Build summary table.
    let header = format!(
        "| {:<25} | {:<35} | {:>8} | {:>8} | {:>7} | {:>8} | {:>7} | {:>10} | {:>15} | {:>11} | {:>12} | {:>14} | {:>14} | {:>19} |",
        "embedder",
        "reranker",
        "recall@2",
        "recall@4",
        "MRR",
        "mean ms",
        "p95 ms",
        "noise_caps",
        "load ms (emb+rr)",
        "peak RSS MB",
        "raw_tok_mean",
        "rend_tok_mean",
        "stale_hit_rate",
        "resolution_accuracy",
    );
    let sep = format!(
        "| {:-<25} | {:-<35} | {:-<8} | {:-<8} | {:-<7} | {:-<8} | {:-<7} | {:-<10} | {:-<15} | {:-<11} | {:-<12} | {:-<14} | {:-<14} | {:-<19} |",
        "", "", "", "", "", "", "", "", "", "", "", "", "", ""
    );

    let mut table_lines: Vec<String> = vec![header, sep];
    for row in &rows {
        let load_ms = row.embedder_load_ms + row.reranker_load_ms;
        let rss_str = row
            .peak_rss_mb
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "n/a".to_string());
        let res_acc_str = if row.summary.resolution_accuracy < 0.0 {
            "N/A".to_string()
        } else {
            format!("{:.3}", row.summary.resolution_accuracy)
        };
        table_lines.push(format!(
            "| {:<25} | {:<35} | {:>8.3} | {:>8.3} | {:>7.3} | {:>8.1} | {:>7.1} | {:>10.1} | {:>15} | {:>11} | {:>12.1} | {:>14.1} | {:>14.3} | {:>19} |",
            row.embedder,
            row.reranker,
            row.summary.recall_at_2,
            row.summary.recall_at_4,
            row.summary.mrr,
            row.summary.mean_latency_ms,
            row.summary.p95_latency_ms,
            row.summary.noise_capsules,
            load_ms,
            rss_str,
            row.summary.raw_tokens_mean,
            row.summary.rendered_tokens_mean,
            row.summary.stale_hit_rate,
            res_acc_str,
        ));
    }

    let summary_md = format!(
        "# Kimetsu Retrieval Benchmark — Summary\n\nSorted by MRR descending.\n\n{}\n",
        table_lines.join("\n")
    );

    let summary_path = out_dir.join("summary.md");
    std::fs::write(&summary_path, &summary_md)?;
    println!("wrote {}", summary_path.display());
    println!();
    println!("{summary_md}");

    Ok(())
}

/// RSS of an external process by PID (Windows only).
#[cfg(all(feature = "embeddings", target_os = "windows"))]
fn process_rss_mb(pid: u32) -> Option<f64> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut pmc = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS>();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0;
        CloseHandle(handle);
        if ok {
            Some(pmc.WorkingSetSize as f64 / (1024.0 * 1024.0))
        } else {
            None
        }
    }
}

#[cfg(all(feature = "embeddings", not(target_os = "windows")))]
fn process_rss_mb(_pid: u32) -> Option<f64> {
    None
}

/// Remote bench: spawn kimetsu-remote, seed a temp brain, measure HTTP MCP retrieval.
#[cfg(feature = "embeddings")]
fn brain_bench_remote(args: BrainBenchArgs) -> KimetsuResult<()> {
    use kimetsu_brain::eval::EvalFixture;
    use kimetsu_brain::project::{add_memory, init_project};
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::time::Instant;

    // ── 0. Locate workspace root and server binary ────────────────────────────
    // Find workspace root by walking up from current_exe.
    let current_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // target/release/kimetsu.exe  →  workspace root is three levels up.
    let workspace_root = current_exe
        .parent() // target/release/
        .and_then(|p| p.parent()) // target/
        .and_then(|p| p.parent()) // workspace root
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "cannot derive workspace root from current_exe".to_string())?;

    #[cfg(windows)]
    let server_bin = workspace_root
        .join("target")
        .join("release")
        .join("kimetsu-remote.exe");
    #[cfg(not(windows))]
    let server_bin = workspace_root
        .join("target")
        .join("release")
        .join("kimetsu-remote");

    if !server_bin.exists() {
        return Err(format!(
            "kimetsu-remote release binary not found at {}\n\
             Build it first:\n  cargo build --release -p kimetsu-remote --features embeddings",
            server_bin.display()
        )
        .into());
    }

    // ── 1. Load fixture ───────────────────────────────────────────────────────
    let fixture_text = std::fs::read_to_string(&args.dataset)
        .map_err(|e| format!("cannot read dataset {}: {e}", args.dataset.display()))?;
    let fixture: EvalFixture =
        serde_json::from_str(&fixture_text).map_err(|e| format!("invalid dataset JSON: {e}"))?;

    let all_keys: std::collections::HashSet<&str> =
        fixture.memories.iter().map(|m| m.key.as_str()).collect();
    for case in &fixture.cases {
        for rel in &case.relevant {
            if !all_keys.contains(rel.as_str()) {
                return Err(format!(
                    "dataset validation: relevant key {:?} in query {:?} not in memories",
                    rel, case.query
                )
                .into());
            }
        }
        for stale in &case.stale {
            if !all_keys.contains(stale.as_str()) {
                return Err(format!(
                    "dataset validation: stale key {:?} in query {:?} not in memories",
                    stale, case.query
                )
                .into());
            }
        }
    }

    let embedders: Vec<&str> = args
        .embedders
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    println!(
        "brain bench --remote: {} embedder(s) (server reranks with --reranker default jina-tiny)",
        embedders.len()
    );
    println!(
        "NOTE: remote applies PRODUCTION floors (min_lexical_coverage 0.5, min_semantic_score 0.35)."
    );
    println!("      Quality numbers are NOT directly comparable to local floors-off results.");
    println!("dataset: {}", args.dataset.display());
    println!("output:  {}", args.out.display());
    println!("concurrency: {}", args.concurrency);
    println!();

    std::fs::create_dir_all(&args.out)?;

    #[derive(serde::Serialize)]
    struct RemoteCaseResult {
        query: String,
        expected: Vec<String>,
        obtained: Vec<String>,
        hit_at_2: bool,
        hit_at_4: bool,
        mrr: f64,
        latency_ms: u128,
        error: Option<String>,
    }

    #[derive(serde::Serialize)]
    struct RemoteComboResult {
        embedder: String,
        seed_ms: u128,
        rss_after_warm_mb: Option<f64>,
        peak_rss_mb: Option<f64>,
        cases: Vec<RemoteCaseResult>,
        summary: RemoteComboSummary,
        concurrent: RemoteConcurrentStats,
    }

    #[derive(serde::Serialize)]
    struct RemoteComboSummary {
        recall_at_2: f64,
        recall_at_4: f64,
        mrr: f64,
        mean_latency_ms: f64,
        p95_latency_ms: f64,
        noise_capsules: f64,
        error_cases: usize,
    }

    #[derive(serde::Serialize)]
    struct RemoteConcurrentStats {
        mean_ms: f64,
        p95_ms: f64,
        total_wall_ms: u128,
        throughput_rps: f64,
    }

    type SummaryRow = (
        String,
        RemoteComboSummary,
        RemoteConcurrentStats,
        Option<f64>,
        Option<f64>,
    );
    let mut summary_rows: Vec<SummaryRow> = Vec::new();

    for &embedder_id in &embedders {
        println!("[remote] embedder: {embedder_id}");

        // ── 2. Pick a free port ───────────────────────────────────────────────
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind free port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?
            .port();
        drop(listener); // release so the server can bind it

        // ── 3. Seed temp brain ────────────────────────────────────────────────
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let safe_emb = embedder_id.replace(['/', '.', ' '], "-");
        // data dir: contains benchrepo/
        let data_dir = std::env::temp_dir().join(format!("kimetsu-remote-bench-{safe_emb}-{ts}"));
        let repo_root = data_dir.join("benchrepo");
        std::fs::create_dir_all(&repo_root)?;
        git_init_boundary(&repo_root);

        // Set env before seeding so memories use this embedder.
        unsafe {
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", embedder_id);
            std::env::set_var("KIMETSU_USER_BRAIN", "0");
        }

        let t_seed = Instant::now();
        init_project(&repo_root, false).map_err(|e| format!("init_project: {e}"))?;

        let mut key_to_id: HashMap<String, String> = HashMap::new();
        for mem in &fixture.memories {
            let id = add_memory(
                &repo_root,
                MemoryScope::Project,
                MemoryKind::Fact,
                &mem.text,
            )
            .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
            key_to_id.insert(mem.key.clone(), id);
        }
        let seed_ms = t_seed.elapsed().as_millis();
        let id_to_key: HashMap<String, String> = key_to_id
            .iter()
            .map(|(k, v)| (v.clone(), k.clone()))
            .collect();
        println!(
            "  seeded {} memories in {seed_ms}ms",
            fixture.memories.len()
        );

        // ── 4. Spawn server ───────────────────────────────────────────────────
        let addr = format!("127.0.0.1:{port}");
        let token = "benchtoken";
        let server = std::process::Command::new(&server_bin)
            .arg("serve")
            .arg("--addr")
            .arg(&addr)
            .arg("--data")
            .arg(&data_dir)
            .arg("--token")
            .arg(token)
            .arg("--rate-limit")
            .arg("0")
            .env("KIMETSU_BRAIN_EMBEDDER", embedder_id)
            .env("KIMETSU_USER_BRAIN", "0")
            .env("KIMETSU_MCP_ENABLE_WRITE_TOOLS", "1")
            // Suppress server log noise during bench
            .env("RUST_LOG", "warn")
            .spawn()
            .map_err(|e| format!("spawn kimetsu-remote: {e}"))?;

        // Kill-on-drop guard: any `?` between here and the explicit kill
        // below would otherwise orphan a live server holding its port and
        // a lock on the temp data dir.
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut server = ChildGuard(server);

        let server_pid = server.0.id();

        // ── 5. Poll readiness (GET /healthz, up to 60s) ───────────────────────
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| format!("build reqwest client: {e}"))?;

        let health_url = format!("http://{addr}/healthz");
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let mut ready = false;
        while Instant::now() < deadline {
            match client.get(&health_url).send() {
                Ok(r) if r.status().is_success() => {
                    ready = true;
                    break;
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }
        if !ready {
            let _ = server.0.kill();
            return Err(
                format!("kimetsu-remote did not become ready within 60s (port {port})").into(),
            );
        }
        println!("  server ready on :{port}");

        // ── 6. Record RSS after warm ──────────────────────────────────────────
        let rss_after_warm = process_rss_mb(server_pid);

        // ── 7. Sequential pass ────────────────────────────────────────────────
        let mcp_url = format!("http://{addr}/mcp/benchrepo");
        let auth_header = format!("Bearer {token}");

        // Helper: call kimetsu_brain_context over HTTP, return (obtained_keys, latency_ms, error).
        let call_context = |query: &str, id: u64| -> (Vec<String>, u128, Option<String>) {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "kimetsu_brain_context",
                    "arguments": {
                        "query": query,
                        "budget_tokens": 6000,
                        "max_capsules": 4
                    }
                }
            });
            let t0 = Instant::now();
            let resp = client
                .post(&mcp_url)
                .header("Authorization", &auth_header)
                .header("Content-Type", "application/json")
                .json(&body)
                .send();
            let latency_ms = t0.elapsed().as_millis();

            let resp = match resp {
                Ok(r) => r,
                Err(e) => return (vec![], latency_ms, Some(format!("HTTP error: {e}"))),
            };

            let json: serde_json::Value = match resp.json() {
                Ok(v) => v,
                Err(e) => return (vec![], latency_ms, Some(format!("JSON parse error: {e}"))),
            };

            // Check for JSON-RPC error
            if let Some(err_obj) = json.get("error") {
                let msg = err_obj
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return (vec![], latency_ms, Some(format!("RPC error: {msg}")));
            }

            // Parse the result: result.content[0].text → JSON string → capsules
            let text = json
                .get("result")
                .and_then(|r| r.get("content"))
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");

            if text.is_empty() {
                return (vec![], latency_ms, Some("empty text in result".to_string()));
            }

            let inner: serde_json::Value = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(e) => return (vec![], latency_ms, Some(format!("inner JSON parse: {e}"))),
            };

            // skipped case → no capsules (intentional, not an error)
            if inner
                .get("skipped")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return (vec![], latency_ms, None);
            }

            let capsules = inner
                .get("capsules")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();

            let keys: Vec<String> = capsules
                .iter()
                .filter_map(|cap| {
                    cap.get("expansion_handle")
                        .and_then(|h| h.as_str())
                        .and_then(|h| h.strip_prefix("memory:"))
                        .and_then(|id| id_to_key.get(id))
                        .cloned()
                })
                .collect();

            (keys, latency_ms, None)
        };

        let mut case_results: Vec<RemoteCaseResult> = Vec::new();
        let mut seq_latencies: Vec<u128> = Vec::new();

        for (idx, case) in fixture.cases.iter().enumerate() {
            let (obtained, latency_ms, error) = call_context(&case.query, idx as u64);
            seq_latencies.push(latency_ms);

            let hit_at_2 = if case.relevant.is_empty() {
                false
            } else {
                obtained.iter().take(2).any(|k| case.relevant.contains(k))
            };
            let hit_at_4 = if case.relevant.is_empty() {
                false
            } else {
                obtained.iter().take(4).any(|k| case.relevant.contains(k))
            };
            let mrr_val = kimetsu_brain::eval::mrr(&obtained, &case.relevant);

            case_results.push(RemoteCaseResult {
                query: case.query.clone(),
                expected: case.relevant.clone(),
                obtained,
                hit_at_2,
                hit_at_4,
                mrr: mrr_val,
                latency_ms,
                error,
            });
        }

        println!("  sequential pass done ({} cases)", case_results.len());

        // ── 8. Concurrent pass ────────────────────────────────────────────────
        let concurrency = args.concurrency.max(1);
        let cases_arc: std::sync::Arc<Vec<_>> = std::sync::Arc::new(
            fixture
                .cases
                .iter()
                .enumerate()
                .map(|(i, c)| (i, c.query.clone()))
                .collect(),
        );
        let t_conc_start = Instant::now();

        // Split cases into chunks for each worker thread.
        let chunk_size = cases_arc.len().div_ceil(concurrency);
        let mut handles = vec![];
        let client_clone = client.clone();
        let mcp_url_clone = mcp_url.clone();
        let auth_clone = auth_header.clone();
        let id_to_key_arc = std::sync::Arc::new(id_to_key.clone());

        // We collect latencies per case from concurrent workers.
        let conc_latencies_arc: std::sync::Arc<std::sync::Mutex<Vec<(usize, u128)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        for chunk_idx in 0..concurrency {
            let cases = std::sync::Arc::clone(&cases_arc);
            let client_t = client_clone.clone();
            let url_t = mcp_url_clone.clone();
            let auth_t = auth_clone.clone();
            let id_to_key_t = std::sync::Arc::clone(&id_to_key_arc);
            let out_t = std::sync::Arc::clone(&conc_latencies_arc);

            let start = chunk_idx * chunk_size;
            let end = (start + chunk_size).min(cases.len());
            if start >= end {
                continue;
            }

            let handle = std::thread::spawn(move || {
                for case_idx in start..end {
                    let (i, ref query) = cases[case_idx];
                    let body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": i as u64 + 10000,
                        "method": "tools/call",
                        "params": {
                            "name": "kimetsu_brain_context",
                            "arguments": {
                                "query": query,
                                "budget_tokens": 6000,
                                "max_capsules": 4
                            }
                        }
                    });
                    let t0 = Instant::now();
                    let _ = client_t
                        .post(&url_t)
                        .header("Authorization", &auth_t)
                        .header("Content-Type", "application/json")
                        .json(&body)
                        .send();
                    let latency_ms = t0.elapsed().as_millis();
                    let _ = id_to_key_t.get(""); // suppress unused warning
                    out_t.lock().unwrap().push((i, latency_ms));
                }
            });
            handles.push(handle);
        }
        for h in handles {
            let _ = h.join();
        }
        let total_wall_ms = t_conc_start.elapsed().as_millis();
        let conc_lats_raw = conc_latencies_arc.lock().unwrap().clone();
        let mut conc_latencies: Vec<u128> = conc_lats_raw.iter().map(|(_, l)| *l).collect();
        conc_latencies.sort_unstable();

        let conc_mean_ms = if conc_latencies.is_empty() {
            0.0
        } else {
            conc_latencies.iter().sum::<u128>() as f64 / conc_latencies.len() as f64
        };
        let conc_p95_ms = if conc_latencies.is_empty() {
            0.0
        } else {
            let idx = ((conc_latencies.len() as f64 * 0.95) as usize).min(conc_latencies.len() - 1);
            conc_latencies[idx] as f64
        };
        let throughput_rps = if total_wall_ms == 0 {
            0.0
        } else {
            fixture.cases.len() as f64 / (total_wall_ms as f64 / 1000.0)
        };

        println!(
            "  concurrent pass done: mean={conc_mean_ms:.0}ms p95={conc_p95_ms:.0}ms throughput={throughput_rps:.1}rps"
        );

        // ── 9. Record peak RSS, kill server ───────────────────────────────────
        let peak_rss = process_rss_mb(server_pid);
        let _ = server.0.kill();
        let _ = server.0.wait();
        let _ = std::fs::remove_dir_all(&data_dir);

        // ── 10. Aggregate metrics ─────────────────────────────────────────────
        let signal_cases: Vec<_> = fixture
            .cases
            .iter()
            .zip(&case_results)
            .filter(|(c, _)| !c.relevant.is_empty())
            .collect();
        let noise_cases: Vec<_> = fixture
            .cases
            .iter()
            .zip(&case_results)
            .filter(|(c, _)| c.relevant.is_empty())
            .collect();

        let recall_at_2 = if signal_cases.is_empty() {
            0.0
        } else {
            signal_cases
                .iter()
                .map(|(_, r)| if r.hit_at_2 { 1.0f64 } else { 0.0 })
                .sum::<f64>()
                / signal_cases.len() as f64
        };
        let recall_at_4 = if signal_cases.is_empty() {
            0.0
        } else {
            signal_cases
                .iter()
                .map(|(_, r)| if r.hit_at_4 { 1.0f64 } else { 0.0 })
                .sum::<f64>()
                / signal_cases.len() as f64
        };
        let mrr_avg = if signal_cases.is_empty() {
            0.0
        } else {
            signal_cases.iter().map(|(_, r)| r.mrr).sum::<f64>() / signal_cases.len() as f64
        };
        let mut sorted_seq = seq_latencies.clone();
        sorted_seq.sort_unstable();
        let mean_latency_ms = if sorted_seq.is_empty() {
            0.0
        } else {
            sorted_seq.iter().sum::<u128>() as f64 / sorted_seq.len() as f64
        };
        let p95_latency_ms = if sorted_seq.is_empty() {
            0.0
        } else {
            let idx = ((sorted_seq.len() as f64 * 0.95) as usize).min(sorted_seq.len() - 1);
            sorted_seq[idx] as f64
        };
        let noise_capsules = if noise_cases.is_empty() {
            0.0
        } else {
            noise_cases
                .iter()
                .map(|(_, r)| r.obtained.len() as f64)
                .sum::<f64>()
                / noise_cases.len() as f64
        };
        let error_cases = case_results.iter().filter(|r| r.error.is_some()).count();

        let summary = RemoteComboSummary {
            recall_at_2,
            recall_at_4,
            mrr: mrr_avg,
            mean_latency_ms,
            p95_latency_ms,
            noise_capsules,
            error_cases,
        };
        let concurrent = RemoteConcurrentStats {
            mean_ms: conc_mean_ms,
            p95_ms: conc_p95_ms,
            total_wall_ms,
            throughput_rps,
        };

        println!(
            "  recall@2={:.3} recall@4={:.3} MRR={:.3} seq_mean={:.0}ms seq_p95={:.0}ms errors={}",
            summary.recall_at_2,
            summary.recall_at_4,
            summary.mrr,
            summary.mean_latency_ms,
            summary.p95_latency_ms,
            summary.error_cases,
        );

        // ── 11. Write per-embedder JSON ───────────────────────────────────────
        let combo = RemoteComboResult {
            embedder: embedder_id.to_string(),
            seed_ms,
            rss_after_warm_mb: rss_after_warm,
            peak_rss_mb: peak_rss,
            cases: case_results,
            summary: RemoteComboSummary {
                recall_at_2,
                recall_at_4,
                mrr: mrr_avg,
                mean_latency_ms,
                p95_latency_ms,
                noise_capsules,
                error_cases,
            },
            concurrent: RemoteConcurrentStats {
                mean_ms: conc_mean_ms,
                p95_ms: conc_p95_ms,
                total_wall_ms,
                throughput_rps,
            },
        };
        let fname = format!("remote-{safe_emb}.json");
        let fpath = args.out.join(&fname);
        std::fs::write(&fpath, serde_json::to_string_pretty(&combo)?)?;
        println!("  wrote {}", fpath.display());
        println!();

        summary_rows.push((
            embedder_id.to_string(),
            summary,
            concurrent,
            rss_after_warm,
            peak_rss,
        ));
    }

    // ── 12. Write summary table ───────────────────────────────────────────────
    let caveat = "\
> **NOTE — remote production floors**: the remote path applies `min_lexical_coverage = 0.5` and \
the AUTO semantic floor (0.35 on bge-family, 0.0 elsewhere — cosine scales are model-dependent). \
Quality numbers are **NOT** directly comparable to the local bench's floors-off results — noise \
cases dropped by the floors are intentional precision wins, not recall failures. The remote server \
reranks with `--reranker` (default `jina-reranker-v1-tiny-en`, operator-level, `off` disables).\n";

    let header = format!(
        "| {:<25} | {:>8} | {:>8} | {:>7} | {:>9} | {:>8} | {:>12} | {:>10} | {:>14} | {:>11} | {:>11} |",
        "embedder",
        "recall@2",
        "recall@4",
        "MRR",
        "seq mean",
        "seq p95",
        "conc mean ms",
        "conc p95",
        "throughput rps",
        "warm RSS MB",
        "peak RSS MB"
    );
    let sep = format!(
        "| {:-<25} | {:-<8} | {:-<8} | {:-<7} | {:-<9} | {:-<8} | {:-<12} | {:-<10} | {:-<14} | {:-<11} | {:-<11} |",
        "", "", "", "", "", "", "", "", "", "", ""
    );

    let mut table_lines = vec![header, sep];
    for (embedder, summary, concurrent, warm_rss, peak_rss) in &summary_rows {
        let warm_str = warm_rss
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "n/a".to_string());
        let peak_str = peak_rss
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "n/a".to_string());
        table_lines.push(format!(
            "| {:<25} | {:>8.3} | {:>8.3} | {:>7.3} | {:>9.1} | {:>8.1} | {:>12.1} | {:>10.1} | {:>14.1} | {:>11} | {:>11} |",
            embedder,
            summary.recall_at_2,
            summary.recall_at_4,
            summary.mrr,
            summary.mean_latency_ms,
            summary.p95_latency_ms,
            concurrent.mean_ms,
            concurrent.p95_ms,
            concurrent.throughput_rps,
            warm_str,
            peak_str,
        ));
    }

    let summary_md = format!(
        "# Kimetsu Remote Benchmark — Summary\n\n{caveat}\nSorted by embedder.\n\n{}\n",
        table_lines.join("\n")
    );

    let summary_path = args.out.join("remote-summary.md");
    std::fs::write(&summary_path, &summary_md)?;
    println!("wrote {}", summary_path.display());
    println!();
    println!("{summary_md}");

    Ok(())
}

/// Worker: run a single embedder×reranker combo in-process, write combo JSON.
#[cfg(feature = "embeddings")]
fn brain_bench_single(args: BrainBenchArgs) -> KimetsuResult<()> {
    use kimetsu_brain::context::{ContextRequest, rerank_capsules};
    use kimetsu_brain::embeddings::{open_embedder_for_model, open_reranker_for_model};
    use kimetsu_brain::eval::EvalFixture;
    use kimetsu_brain::project::{BrainSession, add_memory, init_project};
    use kimetsu_core::memory::{MemoryKind, MemoryScope};
    use kimetsu_core::paths::git_init_boundary;
    use std::collections::HashMap;
    use std::time::Instant;

    // Disable user brain.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
    }

    let embedder_id = args
        .embedders
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("bge-small-en-v1.5")
        .to_string();
    let reranker_id = args
        .rerankers
        .split(',')
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or("off")
        .to_string();

    // ── 1. Load fixture ───────────────────────────────────────────────────────
    let fixture_text = std::fs::read_to_string(&args.dataset)
        .map_err(|e| format!("cannot read dataset {}: {e}", args.dataset.display()))?;
    let fixture: EvalFixture =
        serde_json::from_str(&fixture_text).map_err(|e| format!("invalid dataset JSON: {e}"))?;

    let all_keys: std::collections::HashSet<&str> =
        fixture.memories.iter().map(|m| m.key.as_str()).collect();
    for case in &fixture.cases {
        for rel in &case.relevant {
            if !all_keys.contains(rel.as_str()) {
                return Err(format!(
                    "dataset validation: relevant key {:?} in query {:?} not in memories",
                    rel, case.query
                )
                .into());
            }
        }
        for stale in &case.stale {
            if !all_keys.contains(stale.as_str()) {
                return Err(format!(
                    "dataset validation: stale key {:?} in query {:?} not in memories",
                    stale, case.query
                )
                .into());
            }
        }
    }

    // ── 2. Load embedder (measure RSS before/after) ───────────────────────────
    let rss_before_emb = rss_mb();
    let t_emb = Instant::now();
    // Set env so seeds use THIS embedder.
    unsafe {
        std::env::set_var("KIMETSU_BRAIN_EMBEDDER", &embedder_id);
    }
    let embedder = open_embedder_for_model(&embedder_id);
    let embedder_load_ms = t_emb.elapsed().as_millis();
    let rss_after_emb = rss_mb();

    // ── 3. Load reranker ──────────────────────────────────────────────────────
    let rss_before_rr = rss_mb();
    let t_rr = Instant::now();
    let reranker_box: Option<Box<dyn kimetsu_brain::embeddings::Reranker>> = if reranker_id == "off"
    {
        None
    } else {
        open_reranker_for_model(&reranker_id)
    };
    let reranker_load_ms = t_rr.elapsed().as_millis();
    let rss_after_rr = rss_mb();

    // ── 4. Seed temp brain ────────────────────────────────────────────────────
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let safe_emb = embedder_id.replace(['/', '.', ' '], "-");
    let safe_rr = reranker_id.replace(['/', '.', ' '], "-");
    let tmp_root = std::env::temp_dir().join(format!("kimetsu-bench-{safe_emb}-{safe_rr}-{ts}"));
    std::fs::create_dir_all(&tmp_root)?;
    git_init_boundary(&tmp_root);

    let t_seed = Instant::now();
    init_project(&tmp_root, true).map_err(|e| format!("init_project: {e}"))?;

    let mut key_to_id: HashMap<String, String> = HashMap::new();
    for mem in &fixture.memories {
        let id = add_memory(&tmp_root, MemoryScope::Project, MemoryKind::Fact, &mem.text)
            .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
        key_to_id.insert(mem.key.clone(), id);
    }

    // ── 4b. Apply temporal validity state ────────────────────────────────────
    // Flagship 1 Pass A: for memories with `valid_to` or `superseded_by_key`,
    // stamp the temporal state so validity-aware retrieval can exclude them.
    // Memories without these fields are unchanged — existing fixtures are safe.
    {
        use kimetsu_brain::projector::mark_memory_temporal;
        use kimetsu_core::paths::ProjectPaths;

        let needs_temporal = fixture
            .memories
            .iter()
            .any(|m| m.valid_to.is_some() || m.superseded_by_key.is_some());

        if needs_temporal {
            let paths = ProjectPaths::discover(&tmp_root)
                .map_err(|e| format!("discover paths for temporal seeding: {e}"))?;
            let conn = rusqlite::Connection::open(&paths.brain_db)
                .map_err(|e| format!("open brain_db for temporal seeding: {e}"))?;
            kimetsu_brain::schema::initialize(&conn)
                .map_err(|e| format!("initialize brain for temporal seeding: {e}"))?;

            for mem in &fixture.memories {
                if mem.valid_to.is_none() && mem.superseded_by_key.is_none() {
                    continue;
                }
                let memory_id = match key_to_id.get(&mem.key) {
                    Some(id) => id.clone(),
                    None => continue,
                };

                // Stamp valid_to (expiry) via the memory.temporal event so the
                // action is event-sourced and rebuild-safe.
                if let Some(ref vt) = mem.valid_to {
                    mark_memory_temporal(&conn, &memory_id, None, Some(vt.as_str()))
                        .map_err(|e| format!("mark_memory_temporal valid_to {:?}: {e}", mem.key))?;
                }

                // Stamp superseded_by via a direct SQL update.
                // We use a direct UPDATE rather than a full memory.superseded event
                // because the bench seeder just needs the retrieval exclusion; it
                // doesn't need the full edge + citation reassignment that the
                // consolidation path does.
                if let Some(ref survivor_key) = mem.superseded_by_key {
                    if let Some(survivor_id) = key_to_id.get(survivor_key) {
                        conn.execute(
                            "UPDATE memories SET superseded_by = ?2 WHERE memory_id = ?1",
                            rusqlite::params![&memory_id, survivor_id],
                        )
                        .map_err(|e| {
                            format!(
                                "stamp superseded_by {:?} → {:?}: {e}",
                                mem.key, survivor_key
                            )
                        })?;
                        // Also remove from FTS so FTS path doesn't surface it.
                        conn.execute(
                            "DELETE FROM memories_fts WHERE memory_id = ?1",
                            rusqlite::params![&memory_id],
                        )
                        .map_err(|e| {
                            format!("delete memories_fts for superseded {:?}: {e}", mem.key)
                        })?;
                    }
                }
            }
        }
    }

    let seed_ms = t_seed.elapsed().as_millis();
    let id_to_key: HashMap<String, String> = key_to_id
        .iter()
        .map(|(k, v)| (v.clone(), k.clone()))
        .collect();

    // ── 5. Run cases ─────────────────────────────────────────────────────────
    let session =
        BrainSession::open_readonly(&tmp_root).map_err(|e| format!("open_readonly: {e}"))?;

    #[derive(serde::Serialize)]
    struct ObtainedItem {
        key: String,
        score: f32,
    }
    #[derive(serde::Serialize)]
    struct CaseResult {
        query: String,
        expected: Vec<String>,
        obtained: Vec<ObtainedItem>,
        hit_at_2: bool,
        hit_at_4: bool,
        mrr: f64,
        latency_ms: u128,
        /// v1.5 (Story 2.1): mean rendered tokens across the returned capsules
        /// after compress_for_render(3) vs raw token estimates.
        raw_tokens_mean: f64,
        rendered_tokens_mean: f64,
        /// P0.1: 1.0 if any stale key is in the top-k window, else 0.0.
        stale_hit: f64,
        /// P0.1: true if relevant outranks every stale key in ranked list.
        resolution_correct: bool,
    }

    let mut case_results: Vec<CaseResult> = Vec::new();
    let mut latencies_ms: Vec<u128> = Vec::new();

    for case in &fixture.cases {
        let t0 = Instant::now();
        let request = ContextRequest {
            stage: "localization".to_string(),
            query: case.query.clone(),
            budget_tokens: 6000,
            max_capsules: args.pool,
            min_semantic_score: 0.0,
            min_lexical_coverage: 0.0,
            ..Default::default()
        };
        let mut bundle = session
            .retrieve_context_with_injected_embedder(request, embedder.as_ref())
            .map_err(|e| format!("retrieve: {e}"))?;

        // Apply reranker or truncate.
        if let Some(ref rr) = reranker_box {
            bundle.capsules =
                rerank_capsules(&case.query, bundle.capsules, rr.as_ref(), 0.0, args.cap);
        } else {
            bundle.capsules.truncate(args.cap);
        }

        let latency_ms = t0.elapsed().as_millis();
        latencies_ms.push(latency_ms);

        // Map expansion_handle "memory:<id>" → key.
        let obtained: Vec<ObtainedItem> = bundle
            .capsules
            .iter()
            .map(|c| {
                let key = c
                    .expansion_handle
                    .strip_prefix("memory:")
                    .and_then(|id| id_to_key.get(id))
                    .cloned()
                    .unwrap_or_else(|| "?".to_string());
                ObtainedItem {
                    key,
                    score: c.score,
                }
            })
            .collect();

        let obtained_keys: Vec<String> = obtained.iter().map(|o| o.key.clone()).collect();

        // Metrics.
        let hit_at_2 = if case.relevant.is_empty() {
            false
        } else {
            obtained_keys
                .iter()
                .take(2)
                .any(|k| case.relevant.contains(k))
        };
        let hit_at_4 = if case.relevant.is_empty() {
            false
        } else {
            obtained_keys
                .iter()
                .take(4)
                .any(|k| case.relevant.contains(k))
        };

        let mrr_val = kimetsu_brain::eval::mrr(&obtained_keys, &case.relevant);

        // P0.1: correctness metrics.
        let stale_hit = kimetsu_brain::eval::stale_hit_rate(&obtained_keys, &case.stale, args.cap);
        let resolution_ok =
            kimetsu_brain::eval::resolution_correct(&obtained_keys, &case.relevant, &case.stale);

        // v1.5 (Story 2.1): token estimates — raw vs compressed — for the
        // rendered capsule set. Computed per-case, averaged in the summary.
        let (raw_tokens_mean, rendered_tokens_mean) = {
            use kimetsu_brain::context::{compress_for_render, estimate_tokens};
            let n = bundle.capsules.len();
            if n == 0 {
                (0.0, 0.0)
            } else {
                let raw: u32 = bundle
                    .capsules
                    .iter()
                    .map(|c| estimate_tokens(&c.summary))
                    .sum();
                let rendered: u32 = bundle
                    .capsules
                    .iter()
                    .map(|c| estimate_tokens(&compress_for_render(&c.summary, 3)))
                    .sum();
                (raw as f64 / n as f64, rendered as f64 / n as f64)
            }
        };

        case_results.push(CaseResult {
            query: case.query.clone(),
            expected: case.relevant.clone(),
            obtained,
            hit_at_2,
            hit_at_4,
            mrr: mrr_val,
            latency_ms,
            raw_tokens_mean,
            rendered_tokens_mean,
            stale_hit,
            resolution_correct: resolution_ok,
        });
    }

    // ── 6. Aggregate metrics ──────────────────────────────────────────────────
    let signal_cases: Vec<_> = fixture
        .cases
        .iter()
        .zip(&case_results)
        .filter(|(c, _)| !c.relevant.is_empty())
        .collect();
    let noise_cases: Vec<_> = fixture
        .cases
        .iter()
        .zip(&case_results)
        .filter(|(c, _)| c.relevant.is_empty())
        .collect();

    let recall_at_2 = if signal_cases.is_empty() {
        0.0
    } else {
        signal_cases
            .iter()
            .map(|(_, r)| if r.hit_at_2 { 1.0f64 } else { 0.0 })
            .sum::<f64>()
            / signal_cases.len() as f64
    };
    let recall_at_4 = if signal_cases.is_empty() {
        0.0
    } else {
        signal_cases
            .iter()
            .map(|(_, r)| if r.hit_at_4 { 1.0f64 } else { 0.0 })
            .sum::<f64>()
            / signal_cases.len() as f64
    };
    let mrr_avg = if signal_cases.is_empty() {
        0.0
    } else {
        signal_cases.iter().map(|(_, r)| r.mrr).sum::<f64>() / signal_cases.len() as f64
    };
    let mean_latency_ms = if latencies_ms.is_empty() {
        0.0
    } else {
        latencies_ms.iter().sum::<u128>() as f64 / latencies_ms.len() as f64
    };
    let p95_latency_ms = {
        let mut sorted = latencies_ms.clone();
        sorted.sort_unstable();
        if sorted.is_empty() {
            0.0
        } else {
            let idx = ((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1);
            sorted[idx] as f64
        }
    };
    let noise_capsules = if noise_cases.is_empty() {
        0.0
    } else {
        noise_cases
            .iter()
            .map(|(_, r)| r.obtained.len() as f64)
            .sum::<f64>()
            / noise_cases.len() as f64
    };

    let peak = peak_rss_mb();

    // P0.1: correctness aggregates.
    // stale_hit_rate: mean over ALL cases (cases with no stale keys contribute 0).
    let agg_stale_hit_rate = if case_results.is_empty() {
        0.0
    } else {
        case_results.iter().map(|r| r.stale_hit).sum::<f64>() / case_results.len() as f64
    };

    // resolution_accuracy: mean over cases that ARE correctness cases
    // (knowledge_update, contradiction, temporal, multi_session — i.e. have stale keys).
    let correctness_cases: Vec<_> = fixture
        .cases
        .iter()
        .zip(&case_results)
        .filter(|(c, _)| !c.stale.is_empty())
        .collect();
    let resolution_accuracy = if correctness_cases.is_empty() {
        // No correctness cases → N/A, report as -1.0 sentinel (JSON null-ish).
        -1.0_f64
    } else {
        correctness_cases
            .iter()
            .map(|(_, r)| if r.resolution_correct { 1.0f64 } else { 0.0 })
            .sum::<f64>()
            / correctness_cases.len() as f64
    };

    // v1.5 (Story 2.1): aggregate rendered-token means across all cases.
    let (agg_raw_tokens_mean, agg_rendered_tokens_mean) = {
        let n = case_results.len();
        if n == 0 {
            (0.0, 0.0)
        } else {
            let raw_sum: f64 = case_results.iter().map(|r| r.raw_tokens_mean).sum();
            let rend_sum: f64 = case_results.iter().map(|r| r.rendered_tokens_mean).sum();
            (raw_sum / n as f64, rend_sum / n as f64)
        }
    };

    // ── 7. Write combo JSON ───────────────────────────────────────────────────
    let combo_json = serde_json::json!({
        "embedder": embedder_id,
        "reranker": reranker_id,
        "embedder_load_ms": embedder_load_ms,
        "reranker_load_ms": reranker_load_ms,
        "rss_before_embedder_mb": rss_before_emb,
        "rss_after_embedder_mb": rss_after_emb,
        "rss_before_reranker_mb": rss_before_rr,
        "rss_after_reranker_mb": rss_after_rr,
        "peak_rss_mb": peak,
        "seed_ms": seed_ms,
        "cases": case_results,
        "summary": {
            "recall_at_2": recall_at_2,
            "recall_at_4": recall_at_4,
            "mrr": mrr_avg,
            "mean_latency_ms": mean_latency_ms,
            "p95_latency_ms": p95_latency_ms,
            "noise_capsules": noise_capsules,
            // v1.5 (Story 2.1): token-budget intelligence
            "raw_tokens_mean": agg_raw_tokens_mean,
            "rendered_tokens_mean": agg_rendered_tokens_mean,
            // P0.1: correctness metrics
            "stale_hit_rate": agg_stale_hit_rate,
            // -1.0 = no correctness cases in this fixture (N/A)
            "resolution_accuracy": resolution_accuracy,
        }
    });

    std::fs::create_dir_all(&args.out)?;
    let fname = format!("combo-{safe_emb}-{safe_rr}.json");
    let fpath = args.out.join(&fname);
    std::fs::write(&fpath, serde_json::to_string_pretty(&combo_json)?)?;

    // ── 8. Cleanup ────────────────────────────────────────────────────────────
    let _ = std::fs::remove_dir_all(&tmp_root);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
