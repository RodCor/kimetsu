use std::env;
use std::path::PathBuf;
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
    /// in v0.1 — see SWEBENCH.md for the full integration plan.
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
    /// subscription — cost is reported as a metric, not a hard constraint.
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
    }
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
                println!(
                    "{} [{}:{} confidence={:.2} uses={}] {}",
                    memory.memory_id,
                    memory.scope,
                    memory.kind,
                    memory.confidence,
                    memory.use_count,
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
                if let Some(reason) = proposal.decided_reason.as_deref() {
                    if !reason.is_empty() {
                        println!("  decided_reason: {reason}");
                    }
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
    }
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
            return Ok(());
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
