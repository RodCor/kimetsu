use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use clap::Parser;
use kimetsu_agent::claude_code::ClaudeCodeProvider;
use kimetsu_agent::harness::{KimetsuAgentOpts, run_model_agent};
use kimetsu_agent::tools::{ShellExecutor, ToolRuntime, ToolRuntimeConfig};
use kimetsu_brain::{benchmark, project};
use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::ids::RunId;
use kimetsu_harbor_rs::{AgentDoneParams, HarborSession, HarborShellExecutor, run_multi_step_stub};

#[derive(Debug, Parser)]
#[command(name = "kimetsu-harbor-agent")]
#[command(about = "Terminal-Bench / Harbor adapter binary for Kimetsu")]
#[command(version)]
struct Args {
    /// The Terminal-Bench instruction string the agent should work on.
    #[arg(long)]
    task: String,
    /// Run the protocol-only multi-step stub instead of the real model
    /// agent. Useful for adapter smoke tests without API credentials.
    #[arg(long)]
    stub: bool,
    /// Hard cap on model/tool rounds before agent.done is forced.
    #[arg(long, default_value_t = kimetsu_agent::harness::DEFAULT_MODEL_TURN_BUDGET)]
    turn_budget: u32,
    /// Model id passed to the claude_code provider.
    #[arg(long)]
    model: Option<String>,
    /// Kimetsu project whose broker context should be injected. Also
    /// honors KIMETSU_HARBOR_PROJECT when omitted.
    #[arg(long)]
    project: Option<PathBuf>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn run() -> KimetsuResult<()> {
    let args = Args::parse();
    run_harbor_agent(args)
}

fn run_harbor_agent(args: Args) -> KimetsuResult<()> {
    // Harbor mode has no meaningful host workspace. The ToolRuntime still
    // needs a repo root for artifact bookkeeping; every shell command is
    // proxied through HarborShellExecutor into the benchmark environment.
    let scratch = std::env::temp_dir().join(format!("kimetsu-harbor-{}", RunId::new()));
    std::fs::create_dir_all(&scratch)?;

    let stdin = io::stdin();
    let reader = stdin.lock();
    let stdout = io::stdout();
    let writer = stdout.lock();
    let session = Rc::new(RefCell::new(HarborSession::new(reader, writer)));

    let executor: Box<dyn ShellExecutor> = Box::new(HarborShellExecutor::new(Rc::clone(&session)));

    let result = {
        let mut runtime = ToolRuntime::new(&scratch, RunId::new())?
            .with_shell_executor(executor)
            .with_config(ToolRuntimeConfig {
                redact_secrets: false,
                ..ToolRuntimeConfig::default()
            });

        if args.stub {
            let _ = run_multi_step_stub(&args.task, Rc::clone(&session), &mut runtime)?;
        } else {
            let mut provider = build_harbor_model_provider(args.model.as_deref(), &scratch)?;
            let brain_context = resolve_brain_context(args.project.as_deref(), &args.task)?;
            let report = run_model_agent(
                &args.task,
                &mut runtime,
                &mut *provider,
                KimetsuAgentOpts {
                    turn_budget: args.turn_budget,
                    ..KimetsuAgentOpts::for_harbor()
                },
                brain_context.as_deref(),
            )?;
            session.borrow_mut().emit_done(AgentDoneParams {
                summary: report.summary,
                context: Some(report.context),
            })?;
        }
        Ok(())
    };

    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn build_harbor_model_provider(
    model_override: Option<&str>,
    scratch: &Path,
) -> KimetsuResult<Box<dyn kimetsu_agent::model::ModelProvider>> {
    let oauth = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").map_err(|_| {
        "CLAUDE_CODE_OAUTH_TOKEN is not set; required for kimetsu-harbor-agent model runs (or pass --stub)"
    })?;
    let model_name = model_override
        .map(str::to_string)
        .or_else(|| std::env::var("KIMETSU_HARBOR_MODEL").ok())
        .unwrap_or_else(|| "claude-opus-4-7".to_string());

    let mut config = ProjectConfig::default_for_project("kimetsu-harbor");
    config.model.provider = "claude_code".to_string();
    config.model.model = model_name;
    config.model.api_key_env = "CLAUDE_CODE_OAUTH_TOKEN".to_string();
    config.model.request_timeout_secs = std::env::var("KIMETSU_HARBOR_PROVIDER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1500);
    config.run.max_total_cost_usd = 5.0;

    match ClaudeCodeProvider::from_config_with_key(scratch, &config, Some(&oauth))? {
        Some(provider) => Ok(Box::new(provider)),
        None => Err("failed to construct ClaudeCodeProvider (no API key resolved)".into()),
    }
}

fn resolve_brain_context(cli_path: Option<&Path>, task: &str) -> KimetsuResult<Option<String>> {
    let resolved = cli_path.map(Path::to_path_buf).or_else(|| {
        std::env::var("KIMETSU_HARBOR_PROJECT")
            .ok()
            .map(PathBuf::from)
    });
    let Some(project_dir) = resolved else {
        return Ok(None);
    };
    if !project_dir.is_dir() {
        eprintln!(
            "kimetsu-harbor-agent: --project {} is not a directory; running no-brain",
            project_dir.display()
        );
        return Ok(None);
    }

    let dataset = std::env::var("KIMETSU_HARBOR_DATASET")
        .or_else(|_| std::env::var("HARBOR_DATASET"))
        .unwrap_or_else(|_| benchmark::DEFAULT_BENCHMARK_DATASET.to_string());
    let task_slug = std::env::var("KIMETSU_BENCHMARK_TASK_SLUG").ok();
    let warm_policy = std::env::var("KIMETSU_BRAIN_WARM_POLICY")
        .or_else(|_| std::env::var("KIMETSU_BENCHMARK_WARM_POLICY"))
        .ok()
        .as_deref()
        .and_then(benchmark::BenchmarkWarmPolicy::parse)
        .unwrap_or(benchmark::BenchmarkWarmPolicy::FullWarm);
    let require_benchmark_memory = env_bool("KIMETSU_REQUIRE_BENCHMARK_MEMORY");
    let brain_context = project::retrieve_benchmark_context_readonly(
        &project_dir,
        task,
        &dataset,
        task_slug.as_deref(),
        warm_policy,
        "harbor",
        2500,
        require_benchmark_memory,
        8,
    )
    .map_err(|err| {
        format!(
            "failed to retrieve benchmark brain context from {}: {err}",
            project_dir.display()
        )
    })?;

    if require_benchmark_memory && !brain_context.required_ok {
        return Err(format!(
            "benchmark brain context for {} did not include a task-specific benchmark memory",
            brain_context
                .task_slug
                .as_deref()
                .unwrap_or("<unknown-task>")
        )
        .into());
    }

    Ok(Some(brain_context.playbook_markdown))
}

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}
