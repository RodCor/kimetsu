use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

mod distiller;
mod doctor;
mod harvest_setup;
mod proactive_state;
mod update;

use clap::{Args, Parser, Subcommand};
use kimetsu_agent::bench::{BenchOptions, run_benchmark};
use kimetsu_agent::pipeline::{CodingRunOptions, run_coding};
use kimetsu_agent::swe_bench::{SweBenchOptions, run_swe_bench};
use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "kimetsu")]
#[command(about = "Evidence-first AI coding and research harness")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init(InitArgs),
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Brain {
        #[command(subcommand)]
        command: BrainCommand,
    },
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    Bench {
        #[command(subcommand)]
        command: BenchCommand,
    },
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
    Lock {
        #[command(subcommand)]
        command: LockCommand,
    },
    Bridge {
        #[command(subcommand)]
        command: BridgeCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Interactive REPL chat — kimetsu as a user-facing coding
    /// assistant. Reuses the full agent runtime (tools, prompts, brain,
    /// MP-18 verify) with a stdin/stdout transport. No dependency on
    /// Terminal-Bench.
    Chat(ChatArgs),
    /// Kimetsu doctor — automated wire-health check.
    ///
    /// Validates that every kimetsu subsystem the chat REPL + MCP
    /// sidecar rely on actually works against the current workspace
    /// + user state. Hermetic by default; safe to run in CI.
    ///
    /// Run after upgrading kimetsu, after changing
    /// `KIMETSU_BRAIN_EMBEDDER`, or whenever something looks
    /// off — doctor surfaces the actionable fix.
    Doctor(DoctorArgs),
    /// Check GitHub Releases for a newer Kimetsu and update discovered
    /// local installs.
    Update(UpdateArgs),
    /// Remove discovered Kimetsu executables from this machine.
    Uninstall(UninstallArgs),
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
    /// Confirm removal. Required unless --dry-run is used.
    #[arg(long)]
    yes: bool,
    /// Also remove the user Kimetsu brain directory (~/.kimetsu or
    /// KIMETSU_USER_BRAIN_DIR). Project .kimetsu directories are never removed.
    #[arg(long)]
    delete_user_data: bool,
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
    Scan(BridgeWorkspaceArgs),
    Status(BridgeWorkspaceArgs),
    Import(BridgeImportArgs),
    Export(BridgeExportArgs),
    Sync(BridgeSyncArgs),
    Doctor(BridgeWorkspaceArgs),
}

#[derive(Debug, Args)]
struct BridgeWorkspaceArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Args)]
struct BridgeImportArgs {
    selection: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Args)]
struct BridgeExportArgs {
    selection: String,
    target: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Args)]
struct BridgeSyncArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    Serve(McpServeArgs),
}

#[derive(Debug, Args)]
struct McpServeArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    no_user_skills: bool,
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    Install(PluginInstallArgs),
}

#[derive(Debug, Args)]
struct PluginInstallArgs {
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
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    force: bool,
    /// Skip writing .claude/CLAUDE.md and .claude/settings.json.
    /// Use when you manage Claude Code configuration manually.
    #[arg(long)]
    no_hooks: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Show,
    Edit,
}

#[derive(Debug, Subcommand)]
enum BrainCommand {
    IngestRepo {
        path: PathBuf,
    },
    Search(SearchArgs),
    Context(ContextArgs),
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Rebuild the in-DB memory projection by replaying the event trace.
    /// (Schema upgrades are automatic on open; this does not change the
    /// schema version.)
    Rebuild,
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

#[derive(Debug, Args)]
struct SearchArgs {
    query: String,
    #[arg(long, default_value_t = 10)]
    limit: u32,
}

#[derive(Debug, Args)]
struct ContextArgs {
    query: String,
    #[arg(long, default_value = "localization")]
    stage: String,
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
    Add(MemoryAddArgs),
    List,
    Proposals(ProposalsArgs),
    Accept(AcceptArgs),
    Reject(RejectArgs),
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
    memory_id: String,
    /// Short note persisted alongside invalidated_at; rendered in
    /// `memory list` so the human reviewer remembers why this memory
    /// was retired.
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Debug, Args)]
struct MemoryAddArgs {
    #[arg(long)]
    scope: String,
    #[arg(long, default_value = "fact")]
    kind: String,
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

#[derive(Debug, Subcommand)]
enum RunCommand {
    Coding(CodingArgs),
    Abort { run_id: String },
}

#[derive(Debug, Subcommand)]
enum BenchCommand {
    Run(BenchRunArgs),
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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    keep_fixtures: bool,
    #[arg(long)]
    model_backed: bool,
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
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    allow_high_risk: bool,
    #[arg(long)]
    no_model: bool,
    #[arg(long)]
    no_broker: bool,
    #[arg(long)]
    no_redact: bool,
    #[arg(long)]
    debug: bool,
    task: String,
}

#[derive(Debug, Subcommand)]
enum RunsCommand {
    List,
    Show { run_id: String },
}

#[derive(Debug, Subcommand)]
enum LockCommand {
    Clear {
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
    let filter = EnvFilter::try_from_env("KIMETSU_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

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
        delete_user_data: args.delete_user_data,
    })
}

/// v0.4.6: `kimetsu doctor` entry point. Runs the full health
/// suite + prints either the human or JSON report.
///
/// Exit codes:
///   0 — all checks passed or warned.
///   1 — at least one Fail.
///   2 — internal doctor error (couldn't even run the checks).
fn doctor_cmd(args: DoctorArgs) -> KimetsuResult<()> {
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

fn plugin(command: PluginCommand) -> KimetsuResult<()> {
    use kimetsu_chat::{BridgeTarget, InstallScope, PluginMode, plugin_install};

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
            println!(
                "installed Kimetsu plugin surface for {} ({} scope) in {} mode",
                report.target.as_str(),
                report.scope.as_str(),
                report.mode.as_str()
            );
            for file in report.files {
                println!("  {}", file.display());
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
        }
    }
    Ok(())
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

    println!("project_id: {}", summary.project_id);
    println!("repo_root: {}", summary.repo_root.display());
    println!("kimetsu_dir: {}", summary.kimetsu_dir.display());
    println!("brain_db: {}", summary.brain_db.display());
    println!("model: {}", summary.model);
    println!(
        "project_toml: {}",
        if summary.wrote_project_toml {
            "written"
        } else {
            "kept existing"
        }
    );
    println!(
        "api_key: {} ({})",
        if summary.api_key_present {
            "present"
        } else {
            "missing"
        },
        summary.api_key_env
    );

    if !summary.api_key_present {
        println!(
            "hint: set {} before running model-backed commands",
            summary.api_key_env
        );
    }

    if !args.no_hooks {
        write_claude_hooks(&summary.repo_root)?;
    }

    Ok(())
}

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

const CLAUDE_SETTINGS_CONTENT: &str = r#"{
  "hooks": {
    "UserPromptSubmit": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "kimetsu brain context-hook"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "kimetsu brain pretool-hook"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "kimetsu brain posttool-hook"
          }
        ]
      }
    ],
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "kimetsu brain stop-hook"
          }
        ]
      }
    ]
  }
}
"#;

fn write_claude_hooks(repo_root: &std::path::Path) -> KimetsuResult<()> {
    let claude_dir = repo_root.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;

    let claude_md = claude_dir.join("CLAUDE.md");
    if !claude_md.exists() {
        std::fs::write(&claude_md, CLAUDE_MD_CONTENT)?;
        println!("claude_hooks: wrote {}", claude_md.display());
    } else {
        println!("claude_hooks: kept existing {}", claude_md.display());
    }

    let settings_json = claude_dir.join("settings.json");
    if !settings_json.exists() {
        std::fs::write(&settings_json, CLAUDE_SETTINGS_CONTENT)?;
        println!("claude_hooks: wrote {}", settings_json.display());
    } else {
        println!("claude_hooks: kept existing {}", settings_json.display());
    }

    Ok(())
}

fn config(command: ConfigCommand) -> KimetsuResult<()> {
    match command {
        ConfigCommand::Show => {
            print!("{}", project::config_text(&env::current_dir()?)?);
            Ok(())
        }
        ConfigCommand::Edit => not_implemented("config edit"),
    }
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
            let (effective_query, ambient_payload) =
                if !args.no_ambient && kimetsu_brain::ambient::ambient_enabled() {
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
        BrainCommand::Rebuild => {
            let events = project::rebuild_projection(&env::current_dir()?)?;
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
    domain_list.sort_by(|a, b| b.1.cmp(&a.1));
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

    let bundle = match project::retrieve_context_readonly_with_request(&workspace, request.clone())
    {
        Ok(b) => b,
        Err(_) => return Ok(()), // Brain not initialized — silent fail
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
            .splitn(3, " - ")
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
        println!(
            "[Kimetsu] {} lesson{} recorded this session.",
            recorded,
            if recorded == 1 { "" } else { "s" }
        );
        return Ok(());
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
    let state_path = paths
        .as_ref()
        .map(|p| proactive_state::session_path(&p.kimetsu_dir, sid));

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
        let state_path =
            state_path.unwrap_or_else(|| proactive_state::session_path(&paths.kimetsu_dir, sid));
        let mut state = proactive_state::load(&state_path);
        if !state.harvest_cued() {
            println!(
                "[kimetsu-harvest] No lessons recorded this non-trivial session. If anything \
                 durable was learned, run the kimetsu-memory-harvester agent in the background \
                 to capture it — otherwise call kimetsu_brain_record."
            );
            state.note_harvest_cue(proactive_state::now_unix());
            proactive_state::save(&state_path, &state);
            return Ok(());
        }
    }

    println!(
        "[Kimetsu] No lessons recorded. After non-trivial solutions, call kimetsu_brain_record."
    );
    Ok(())
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
    proactive_state::gc(&paths.kimetsu_dir, now);

    let state_path = proactive_state::session_path(&paths.kimetsu_dir, hook.session_id.as_deref());
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
        .splitn(3, " - ")
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
        RunCommand::Abort { run_id: _ } => not_implemented("run abort"),
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
    }
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

fn not_implemented(feature: &str) -> KimetsuResult<()> {
    println!("{feature} is planned but not implemented in phase 0");
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
}
