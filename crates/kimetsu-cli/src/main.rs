use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

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
    /// v0.3: interactive REPL chat - kimetsu as a user-facing coding
    /// assistant. Reuses the full agent runtime (tools, prompts, brain,
    /// MP-18 verify) with a stdin/stdout transport. No dependency on
    /// Terminal-Bench.
    Chat(ChatArgs),
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
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long)]
    force: bool,
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
    Rebuild,
    Stats,
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
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    Add(MemoryAddArgs),
    List,
    Proposals(ProposalsArgs),
    Accept(AcceptArgs),
    Reject(RejectArgs),
    Invalidate(InvalidateArgs),
    /// MP-5a: batch review pending memory proposals. The v0.2 default is
    /// "human curates"; this subcommand is the non-interactive batch mode
    /// (interactive TTY review lands in MP-5b).
    Review(ReviewArgs),
    /// MP-6: ranked memories by usefulness ratio so the user can see what
    /// is pulling weight after curation.
    Top(TopArgs),
    /// MP-6: bulk-invalidate memories whose outcome attribution says they
    /// hurt more than they help. Safe-by-default: dry-run unless --apply.
    Prune(PruneArgs),
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
    /// in v0.1 â€” see docs/SWEBENCH.md for the full integration plan.
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
    }
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
    use kimetsu_chat::{BridgeTarget, plugin_install};

    match command {
        PluginCommand::Install(args) => {
            let workspace = args.workspace.canonicalize()?;
            let target = BridgeTarget::parse(&args.target)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            let report = plugin_install(&workspace, target, args.force)
                .map_err(|err| format!("kimetsu plugin install: {err}"))?;
            println!(
                "installed Kimetsu plugin surface for {}",
                report.target.as_str()
            );
            for file in report.files {
                println!("  {}", file.display());
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
    let summary = project::init_project(&env::current_dir()?, args.force)?;

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
            let bundle = project::retrieve_context(
                &env::current_dir()?,
                &args.stage,
                &args.query,
                args.budget_tokens,
            )?;
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
    }
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

    /// MP-5b: end-to-end driver test for the interactive loop. Inject three
    /// pending proposals, script `a\nr\nbecause noisy\ns\n` as stdin input,
    /// confirm: one proposal becomes a memory, one is rejected with the
    /// typed reason, one stays pending; summary line accounts for all three.
    #[test]
    fn interactive_loop_accepts_rejects_and_skips_from_scripted_input() {
        // ulid-named temp dir to avoid collisions when tests run concurrently.
        let root = std::env::temp_dir().join(format!("kimetsu-cli-test-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create temp project");
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
        let root = std::env::temp_dir().join(format!("kimetsu-cli-test-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create temp project");
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
}
