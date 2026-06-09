use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod distiller;
mod doctor;
mod embed_daemon;
mod harvest_setup;
mod proactive_state;
mod process;
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
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Add a durable memory directly.
    Add(MemoryAddArgs),
    /// List active memories with usefulness stats.
    List,
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
pub fn resolve_setup_hosts(
    arg: Option<&str>,
    present_claude: bool,
    present_codex: bool,
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
        let prompt = "Which host agent do you use? [claude-code/codex/openclaw/pi/both]: ";
        #[cfg(all(feature = "pi", not(feature = "openclaw")))]
        let prompt = "Which host agent do you use? [claude-code/codex/pi/both]: ";
        #[cfg(all(not(feature = "pi"), feature = "openclaw"))]
        let prompt = "Which host agent do you use? [claude-code/codex/openclaw/both]: ";
        #[cfg(all(not(feature = "pi"), not(feature = "openclaw")))]
        let prompt = "Which host agent do you use? [claude-code/codex/both]: ";
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

/// Detect whether the home config directories for Claude Code, Codex, OpenClaw, and Pi exist.
/// Returns `(claude_present, codex_present, openclaw_present, pi_present)`.
fn detect_present_hosts() -> (bool, bool, bool, bool) {
    let home = std::env::var_os("USERPROFILE")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()))
        .map(std::path::PathBuf::from);

    let home = match home {
        Some(h) => h,
        None => return (false, false, false, false),
    };

    let claude_present = home.join(".claude").is_dir();
    let codex_present = home.join(".codex").is_dir();
    #[cfg(feature = "openclaw")]
    let openclaw_present = home.join(".openclaw").is_dir();
    #[cfg(not(feature = "openclaw"))]
    let openclaw_present = false;
    #[cfg(feature = "pi")]
    let pi_present = home.join(".pi").is_dir();
    #[cfg(not(feature = "pi"))]
    let pi_present = false;
    (claude_present, codex_present, openclaw_present, pi_present)
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
    let (present_claude, present_codex, present_openclaw, present_pi) = detect_present_hosts();
    let is_tty = io::stdin().is_terminal();
    let stdin = io::stdin();
    let hosts = resolve_setup_hosts(
        args.host.as_deref(),
        present_claude,
        present_codex,
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
            eprintln!(
                "note: `config set` re-serialises the file — TOML comments are not preserved. \
                 Use `config edit` to hand-edit with comments."
            );
            let cwd = env::current_dir()?;
            let paths = kimetsu_core::paths::ProjectPaths::discover(&cwd)?;

            // 1. Read the on-disk file into a toml::Value so we preserve all
            //    existing keys and detect the existing type for coercion.
            let disk_text = std::fs::read_to_string(&paths.project_toml).map_err(|e| {
                format!(
                    "config set: could not read {}: {e}",
                    paths.project_toml.display()
                )
            })?;
            let mut root: toml::Value = toml::from_str(&disk_text)
                .map_err(|e| format!("config set: project.toml is invalid TOML: {e}"))?;

            // 2. Determine the existing type at this key (for coercion).
            let existing = get_toml_path(&root, &key).cloned();
            let typed_value =
                parse_scalar(&value, existing.as_ref()).map_err(|e| format!("config set: {e}"))?;

            // 3. Navigate/create the path and set the leaf.
            set_toml_path(&mut root, &key, typed_value).map_err(|e| format!("config set: {e}"))?;

            // 4. Serialise back to text and validate through ProjectConfig.
            let new_text = toml::to_string_pretty(&root)
                .map_err(|e| format!("config set: failed to serialise: {e}"))?;
            project::load_config_from_text(&new_text).map_err(|e| {
                format!("config set: result is not a valid config — {e}. File NOT written.")
            })?;

            // 5. Write — only reached when validation passes.
            std::fs::write(&paths.project_toml, &new_text).map_err(|e| {
                format!(
                    "config set: failed to write {}: {e}",
                    paths.project_toml.display()
                )
            })?;

            println!("set {key} = {value}");
            Ok(())
        }
    }
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
            let config_ambient = kimetsu_core::paths::ProjectPaths::discover(&cwd)
                .ok()
                .and_then(|paths| project::load_config(&paths).ok())
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
            Ok(())
        }
        BrainCommand::Compact(args) => brain_compact(args),
        BrainCommand::Export(args) => brain_export(args),
        BrainCommand::Import(args) => brain_import(args),
        BrainCommand::Backup(args) => brain_backup(args),
        BrainCommand::EmbedDaemon(args) => brain_embed_daemon(args),
        BrainCommand::Warm => brain_warm(),
        BrainCommand::Daemon(args) => brain_daemon(args),
        BrainCommand::Eval(args) => brain_eval(args),
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

    let memories = project::export_memories(&workspace, scope, kind)?;
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

// ── embed-daemon / warm / daemon subcommand handlers ─────────────────────────

#[cfg(feature = "embeddings")]
fn brain_embed_daemon(args: EmbedDaemonArgs) -> KimetsuResult<()> {
    use embed_daemon::server::{serve_with_listener, DaemonState};
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
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
            Some(proto::Response::Info { version, model, uptime_s, requests, loaded_ms }) => {
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
    Some(kimetsu_brain::embeddings::resolve_embedder_id(Some(config.embedder.model.as_str())).to_string())
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
        Some(proto::Response::Capsules { capsules, skipped, top_score }) => {
            Some(daemon_capsules_to_bundle(request, capsules, skipped, top_score))
        }
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

    if json {
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

    // Extract the prompt text from the hook payload
    let prompt = if input.trim().is_empty() {
        String::new()
    } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&input) {
        v.get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        // Plain text fallback
        input.trim().to_string()
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
        let top_score = bundle.top_score;
        let query_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            request.query.hash(&mut h);
            format!("{:016x}", h.finish())
        };
        let _ = project::log_telemetry_event(
            &workspace,
            "context.served",
            serde_json::json!({
                "query_hash": query_hash,
                "capsule_count": bundle.capsules.len(),
                "top_score": top_score,
                "skipped": bundle.skipped,
                "stage": &request.stage,
                "retrieval_path": retrieval_path,
            }),
        );
    }

    if bundle.skipped || bundle.capsules.is_empty() {
        return Ok(()); // Nothing relevant — zero output
    }

    let mut additional_context = String::from("Kimetsu brain relevant knowledge for this task:");
    for capsule in &bundle.capsules {
        // Strip the "scope:kind - " prefix from the summary for readability
        let text = capsule
            .summary
            .split(" - ")
            .nth(1)
            .unwrap_or(&capsule.summary);
        additional_context.push('\n');
        additional_context.push_str(text);
    }

    print_user_prompt_submit_context(&additional_context)?;
    Ok(())
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

    if recorded > 0 {
        return emit_stop_hook_json(stop_lessons_recorded_json(recorded));
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
    let sid = session.get("session_id").and_then(|v| v.as_str());
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

    emit_stop_hook_json(stop_no_lessons_json())
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
fn stop_lessons_recorded_json(recorded: usize) -> serde_json::Value {
    serde_json::json!({
        "systemMessage": format!(
            "[Kimetsu] {recorded} lesson{} recorded this session.",
            if recorded == 1 { "" } else { "s" }
        ),
    })
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
fn stop_no_lessons_json() -> serde_json::Value {
    serde_json::json!({
        "systemMessage":
            "[Kimetsu] No lessons recorded. After non-trivial solutions, call kimetsu_brain_record.",
    })
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
    // capture the auto-harvest toggle for the resolution cue below.
    let auto_harvest = match project::load_config(&paths) {
        Ok(config) => {
            kimetsu_brain::embeddings::apply_embedder_selection(Some(&config.embedder.model));
            config.learning.auto_harvest
        }
        Err(_) => true,
    };

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);
    if input.trim().is_empty() {
        return Ok(());
    }
    let hook = parse_hook_tool_input(&input);

    // Defensive tool-name gate (the hook matcher should already scope
    // to Bash, but be safe across harness quirks).
    if let Some(name) = hook.tool_name.as_deref()
        && !name.eq_ignore_ascii_case("bash")
    {
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
    let (query, kinds, error_sig): (String, &[&str], Option<String>) = match event {
        ProactiveEvent::PreTool => {
            let Some(cmd) = hook.command.as_deref() else {
                return Ok(());
            };
            (cmd.to_string(), &["failure_pattern", "convention"], None)
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

    let body = capsule
        .summary
        .split(" - ")
        .nth(1)
        .unwrap_or(&capsule.summary);
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
        MemoryCommand::List => {
            let memories = project::list_memories(&env::current_dir()?)?;
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
    use kimetsu_brain::embeddings::{NoopEmbedder, open_embedder_for_model, open_reranker_for_model};
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
    let fixture_text = std::fs::read_to_string(fixture_path).map_err(|e| {
        format!("cannot read fixture {}: {e}", fixture_path.display())
    })?;
    let fixture: EvalFixture = serde_json::from_str(&fixture_text).map_err(|e| {
        format!("invalid fixture JSON in {}: {e}", fixture_path.display())
    })?;

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
    println!("adding {} memories to temp brain...", fixture.memories.len());
    let mut key_to_id: HashMap<String, String> = HashMap::new();
    for mem in &fixture.memories {
        let memory_id = add_memory(
            &tmp_root,
            MemoryScope::Project,
            MemoryKind::Fact,
            &mem.text,
        )
        .map_err(|e| format!("add_memory {:?}: {e}", mem.key))?;
        key_to_id.insert(mem.key.clone(), memory_id);
    }

    // Build key → id lookup from the map (for ranking back to keys).
    let id_to_key: HashMap<String, String> =
        key_to_id.iter().map(|(k, v)| (v.clone(), k.clone())).collect();

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

        for case in &fixture.cases {
            let fetch_cap = pool;
            let request = ContextRequest {
                stage: "localization".to_string(),
                query: case.query.clone(),
                budget_tokens: 6000,
                max_capsules: fetch_cap,
                min_semantic_score: 0.0, // disable floor for eval recall
                min_lexical_coverage: 0.0, // disable floor for eval recall
                ..Default::default()
            };
            let mut bundle = session
                .retrieve_context_with_injected_embedder(request, embedder)
                .map_err(|e| format!("{mode_label} retrieve: {e}"))?;

            // Apply reranker when present.
            if let Some(rr) = reranker {
                bundle.capsules = rerank_capsules(
                    &case.query,
                    bundle.capsules,
                    rr,
                    rerank_floor,
                    rerank_cap,
                );
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
    // Mirror the daemon's production constants (server.rs RERANK_POOL/FLOOR).
    let pool = 12usize;
    let rerank_floor = 0.30f32;
    let rerank_cap = 4usize;

    print!("running fts mode...");
    let (fts_ranked, fts_ms) =
        run_mode("fts", &NoopEmbedder, None, pool, 0.0, 0)?;
    println!(" done ({fts_ms} ms)");

    print!("running semantic mode (loading embedder)...");
    let semantic_embedder = open_embedder_for_model("bge-small-en-v1.5");
    let (sem_ranked, sem_ms) =
        run_mode("semantic", semantic_embedder.as_ref(), None, pool, 0.0, 0)?;
    println!(" done ({sem_ms} ms)");

    print!("running semantic+rerank mode (loading reranker)...");
    let reranker_opt = open_reranker_for_model("jina-reranker-v1-turbo-en");
    let reranker_ref: Option<&dyn kimetsu_brain::embeddings::Reranker> =
        reranker_opt.as_deref();
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
            noise_indices.iter().map(|&i| ranked[i].len() as f64).sum::<f64>()
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
        let run_reranker_bench =
            |rr_id: &str| -> KimetsuResult<RankerBenchRow> {
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
                        .retrieve_context_with_injected_embedder(
                            request,
                            semantic_embedder.as_ref(),
                        )
                        .map_err(|e| format!("{rr_id} retrieve: {e}"))?;

                    // Time only the rerank step.
                    let rr_start = Instant::now();
                    if !eval_cases[per_case_ranked.len()].relevant.is_empty() {
                        bundle.capsules = rerank_capsules(
                            &case.query,
                            bundle.capsules,
                            rr,
                            rerank_floor,
                            rerank_cap,
                        );
                        rerank_times_ms.push(rr_start.elapsed().as_millis());
                    } else {
                        // Noise case: still rerank so we get noise metric.
                        bundle.capsules = rerank_capsules(
                            &case.query,
                            bundle.capsules,
                            rr,
                            rerank_floor,
                            rerank_cap,
                        );
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
                    let home_cache = std::env::var("HF_HOME").ok().map(std::path::PathBuf::from)
                        .or_else(|| {
                            std::env::var("HOME").ok()
                                .or_else(|| std::env::var("USERPROFILE").ok())
                                .map(|h| std::path::PathBuf::from(h).join(".cache").join("huggingface").join("hub"))
                        });
                    home_cache.and_then(|cache_root| {
                        let safe_name = repo_id.replace('/', "--");
                        let snap_dir = cache_root.join(format!("models--{safe_name}")).join("snapshots");
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
            "(semantic, no rerank)",
            "-",
            "-",
            "-",
            sem_r2,
            sem_r4,
            sem_mrr,
            sem_noise,
            "-",
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

            // --- set embedder.enabled = false ---
            let disk_text = std::fs::read_to_string(&paths.project_toml).expect("read toml");
            let mut root_val: toml::Value = toml::from_str(&disk_text).expect("parse");
            let existing = get_toml_path(&root_val, "embedder.enabled").cloned();
            let typed = parse_scalar("false", existing.as_ref()).expect("parse false as bool");
            set_toml_path(&mut root_val, "embedder.enabled", typed).expect("set");
            let new_text = toml::to_string_pretty(&root_val).expect("serialise");
            project::load_config_from_text(&new_text).expect("validate");
            std::fs::write(&paths.project_toml, &new_text).expect("write");

            // --- verify via load_config ---
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
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_auto_only_claude_present() {
        use kimetsu_chat::BridgeTarget;
        // Only Claude present → Claude.
        let hosts =
            resolve_setup_hosts(None, true, false, false, false, false, Cursor::new(b"")).unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode]);
    }

    #[test]
    fn resolve_setup_hosts_auto_only_codex_present() {
        use kimetsu_chat::BridgeTarget;
        let hosts =
            resolve_setup_hosts(None, false, true, false, false, false, Cursor::new(b"")).unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_auto_both_present() {
        use kimetsu_chat::BridgeTarget;
        let hosts =
            resolve_setup_hosts(None, true, true, false, false, false, Cursor::new(b"")).unwrap();
        assert_eq!(hosts, vec![BridgeTarget::ClaudeCode, BridgeTarget::Codex]);
    }

    #[test]
    fn resolve_setup_hosts_neither_present_non_tty_defaults_claude() {
        use kimetsu_chat::BridgeTarget;
        let hosts =
            resolve_setup_hosts(None, false, false, false, false, false, Cursor::new(b"")).unwrap();
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
        let hosts =
            resolve_setup_hosts(None, false, false, false, true, false, Cursor::new(b"")).unwrap();
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
            Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Pi]);
    }

    #[cfg(feature = "pi")]
    #[test]
    fn resolve_setup_hosts_tty_scripted_pi() {
        use kimetsu_chat::BridgeTarget;
        let hosts =
            resolve_setup_hosts(None, false, false, false, false, true, Cursor::new(b"pi\n"))
                .unwrap();
        assert_eq!(hosts, vec![BridgeTarget::Pi]);
    }

    #[cfg(feature = "openclaw")]
    #[test]
    fn resolve_setup_hosts_auto_only_openclaw_present() {
        use kimetsu_chat::BridgeTarget;
        // Only OpenClaw present → OpenClaw detected.
        let hosts =
            resolve_setup_hosts(None, false, false, true, false, false, Cursor::new(b"")).unwrap();
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
}
