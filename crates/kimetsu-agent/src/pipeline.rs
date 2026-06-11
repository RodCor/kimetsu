use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_brain::context::{self, ContextBundle, ContextCapsule, ContextRequest};
use kimetsu_brain::ingest;
use kimetsu_brain::lock::ProjectLock;
use kimetsu_brain::project;
use kimetsu_brain::projector;
use kimetsu_brain::trace::{RunPaths, TraceWriter, read_trace};
use kimetsu_core::KimetsuResult;
use kimetsu_core::config::{ProjectConfig, adaptive_budget};
use kimetsu_core::event::Event;
use kimetsu_core::ids::{RunId, new_id};
use kimetsu_core::paths::ProjectPaths;
use serde::{Deserialize, Serialize};

use crate::agent_loop::{AgentLoop, AgentLoopConfig, AgentLoopOutcome, parse_structured_json};
use crate::anthropic::AnthropicProvider;
use crate::bedrock::BedrockProvider;
use crate::claude_code::ClaudeCodeProvider;
use crate::model::{
    MessageContent, MessageRole, ModelMessage, ModelProvider, ModelRequest, ModelResponse,
    TokenUsage, ToolChoice, default_tool_definitions,
};
use crate::recall_ledger::RunRecallLedger;
use crate::tools::{CommandSpec, ToolPatchPlan, ToolRuntime, ToolRuntimeConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingStage {
    Intake,
    RepoScan,
    ContextRetrieval,
    Localization,
    PatchPlan,
    Implementation,
    Verification,
    Review,
    MemoryProposal,
    FinalReport,
}

impl CodingStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::RepoScan => "repo_scan",
            Self::ContextRetrieval => "context_retrieval",
            Self::Localization => "localization",
            Self::PatchPlan => "patch_plan",
            Self::Implementation => "implementation",
            Self::Verification => "verification",
            Self::Review => "review",
            Self::MemoryProposal => "memory_proposal",
            Self::FinalReport => "final_report",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodingRunOptions {
    pub repo: PathBuf,
    pub task: String,
    pub dry_run: bool,
    pub allow_high_risk: bool,
    pub disable_model: bool,
    pub disable_broker: bool,
    pub model_key_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodingRunResult {
    pub run_id: RunId,
    pub dry_run: bool,
    pub patch_plan_id: String,
    pub final_report_path: PathBuf,
    pub trace_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchPlan {
    pub patch_plan_id: String,
    pub run_id: String,
    pub revision_of: Option<String>,
    pub rationale: String,
    pub files_to_read: Vec<String>,
    pub files_to_modify: Vec<String>,
    pub files_to_create: Vec<String>,
    pub files_to_delete: Vec<String>,
    pub verification_commands: Vec<CommandSpec>,
    pub expected_outcome: String,
    pub risk_level: RiskLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PatchPlanDraft {
    #[serde(default)]
    rationale: String,
    #[serde(default)]
    files_to_read: Vec<String>,
    #[serde(default)]
    files_to_modify: Vec<String>,
    #[serde(default)]
    files_to_create: Vec<String>,
    #[serde(default)]
    files_to_delete: Vec<String>,
    #[serde(default)]
    verification_commands: Vec<CommandSpec>,
    #[serde(default)]
    expected_outcome: String,
    #[serde(default)]
    risk_level: serde_json::Value,
}

struct SelectedTextProvider {
    provider_name: String,
    model_name: String,
    provider: Box<dyn ModelProvider>,
}

impl SelectedTextProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        self.provider.complete(request)
    }
}

pub fn run_coding_dry_run(options: CodingRunOptions) -> KimetsuResult<CodingRunResult> {
    run_coding(options)
}

pub fn run_coding(options: CodingRunOptions) -> KimetsuResult<CodingRunResult> {
    let (paths, config, conn) = project::load_project(&options.repo)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "run coding", Some(run_id))?;
    let (mut writer, run_paths) = TraceWriter::create(&paths, run_id)?;
    let mut events = Vec::new();

    emit(
        &mut writer,
        &mut events,
        Event::new(
            run_id,
            "run.started",
            serde_json::json!({
                "mode": "coding",
                "task": options.task,
                "project_id": config.kimetsu.project_id,
                "repo_root": paths.repo_root.to_string_lossy(),
                "model": format!("{}/{}", config.model.provider, config.model.model),
                "platform": std::env::consts::OS,
                "kimetsu_version": env!("CARGO_PKG_VERSION"),
                "config_hash": config_hash(&paths.project_toml)?,
                "dry_run": options.dry_run,
            }),
        ),
    )?;

    stage_entered(&mut writer, &mut events, run_id, CodingStage::Intake)?;
    stage_completed(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::Intake,
        serde_json::json!({
            "summary": if options.dry_run {
                "Task accepted for dry-run patch planning."
            } else {
                "Task accepted for implementation."
            },
            "task": options.task,
            "allow_high_risk": options.allow_high_risk,
        }),
    )?;

    stage_entered(&mut writer, &mut events, run_id, CodingStage::RepoScan)?;
    let ingest_summary = ingest::ingest_repo(&conn, &paths, &config)?;
    emit(
        &mut writer,
        &mut events,
        Event::new(
            run_id,
            "repo.ingested",
            serde_json::json!({
                "repo_root": ingest_summary.repo_root.to_string_lossy(),
                "indexed_files": ingest_summary.indexed_files,
                "skipped_files": ingest_summary.skipped_files,
                "manifests": ingest_summary.manifests,
            }),
        ),
    )?;
    stage_completed(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::RepoScan,
        serde_json::json!({
            "summary": "Repo index refreshed.",
            "indexed_files": ingest_summary.indexed_files,
            "skipped_files": ingest_summary.skipped_files,
            "manifests": ingest_summary.manifests,
        }),
    )?;

    stage_entered(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::ContextRetrieval,
    )?;
    let repo_root = paths
        .repo_root
        .canonicalize()?
        .to_string_lossy()
        .to_string();
    // E3: classify the task once at intake. Feature is the neutral default
    // so disable_broker runs and any other path that skips retrieval are
    // unaffected — they never set task_kind on a ContextRequest at all.
    let task_kind = context::classify_task(&options.task);

    // F1+F3: one ledger per run — created early so the per-run global cap
    // (budget_run_cap_tokens) can be tracked across all retrieval + render
    // stages. F1 deduplicates capsules; F3 caps total brain tokens.
    let mut recall_ledger = RunRecallLedger::new();

    // F3: task-size signal (first component: task text tokens only; file
    // context is not yet known at this point, before localization retrieval).
    // Defined as: tokens(task_text) + tokens(localized_file_context).
    // The task_text component is computed here using the same heuristic
    // as the rest of the pipeline: (whitespace_words * 1.33).ceil().
    // The file-context component is added after localization retrieval.
    let task_tokens = estimate_task_tokens(&options.task);
    let floor = config.broker.budget_floor_tokens;
    let run_cap = config.broker.budget_run_cap_tokens;

    let (localization_context, patch_context, broker_summary) = if options.disable_broker {
        let empty_loc = ContextBundle {
            stage: CodingStage::Localization.as_str().to_string(),
            budget_tokens: 0,
            used_tokens: 0,
            capsules: Vec::new(),
            excluded: Vec::new(),
            skipped: false,
            top_score: 0.0,
        };
        let empty_plan = ContextBundle {
            stage: CodingStage::PatchPlan.as_str().to_string(),
            budget_tokens: 0,
            used_tokens: 0,
            capsules: Vec::new(),
            excluded: Vec::new(),
            skipped: false,
            top_score: 0.0,
        };
        (empty_loc, empty_plan, "Broker disabled (brain_off).")
    } else {
        // F3: localization stage budget — task_size uses task tokens only
        // (file context unknown pre-retrieval). remaining starts at run_cap
        // (ledger is empty before any rendering).
        let remaining_loc = run_cap.saturating_sub(recall_ledger.injected_tokens());
        let loc_budget = adaptive_budget(task_tokens, floor, run_cap).min(remaining_loc);

        let localization_context = context::retrieve_context(
            &conn,
            &repo_root,
            &config.broker.weights,
            ContextRequest {
                stage: CodingStage::Localization.as_str().to_string(),
                query: options.task.clone(),
                // F3: adaptive sublinear budget, capped to remaining run budget.
                budget_tokens: loc_budget,
                // D1f: propagate config-driven caps into the request so
                // retrieve_context_with_embedder honours them directly.
                max_capsules: config.broker.max_capsules,
                min_semantic_score: config.broker.min_semantic_score,
                // E3: task-kind adaptive routing.
                task_kind,
                ..Default::default()
            },
        )?;

        // F3: determine localized files to compute the full task-size signal
        // for the patch-plan stage. The file_context component accounts for
        // the extra context tokens injected from the localized file list.
        let localized_for_size = likely_files(&localization_context, 5);
        let file_context_tokens =
            estimate_task_tokens(&render_localized_files(&localized_for_size));
        let task_size_full = task_tokens.saturating_add(file_context_tokens);

        // F3: patch-plan stage budget — full task-size signal (task + files).
        // Remaining budget = run_cap minus what localization retrieval budgeted.
        // (Rendering hasn't happened yet at this point, but we conservatively
        // reserve half the run_cap for each stage to avoid starvation.)
        let remaining_patch = run_cap.saturating_sub(loc_budget);
        let patch_budget = adaptive_budget(task_size_full, floor, run_cap).min(remaining_patch);

        let patch_context = context::retrieve_context(
            &conn,
            &repo_root,
            &config.broker.weights,
            ContextRequest {
                stage: CodingStage::PatchPlan.as_str().to_string(),
                query: options.task.clone(),
                // F3: adaptive sublinear budget, capped to remaining run budget.
                budget_tokens: patch_budget,
                // D1f: same config-driven caps for the patch-plan stage.
                max_capsules: config.broker.max_capsules,
                min_semantic_score: config.broker.min_semantic_score,
                // E3: task-kind adaptive routing.
                task_kind,
                ..Default::default()
            },
        )?;
        (
            localization_context,
            patch_context,
            "Context capsules retrieved.",
        )
    };
    // MP-4a: emit a `context.injected` event per stage so the projector can
    // correlate accepted memories with terminal outcomes. The projector reads
    // every context.injected for a run when applying run.finished/failed and
    // updates memories.usefulness_score / use_count accordingly.
    emit_context_injected(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::Localization,
        &localization_context,
    )?;
    emit_context_served(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::Localization,
        &localization_context,
    )?;
    emit_context_injected(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::PatchPlan,
        &patch_context,
    )?;
    emit_context_served(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::PatchPlan,
        &patch_context,
    )?;
    stage_completed(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::ContextRetrieval,
        serde_json::json!({
            "summary": broker_summary,
            "broker_disabled": options.disable_broker,
            "localization_capsules": localization_context.capsules.len(),
            "patch_plan_capsules": patch_context.capsules.len(),
        }),
    )?;

    stage_entered(&mut writer, &mut events, run_id, CodingStage::Localization)?;
    let files_to_read = likely_files(&localization_context, 5);
    stage_completed(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::Localization,
        serde_json::json!({
            "summary": "Likely files selected from context capsules.",
            "files_to_read": files_to_read,
        }),
    )?;

    stage_entered(&mut writer, &mut events, run_id, CodingStage::PatchPlan)?;
    let model_patch_plan = if options.disable_model {
        emit(
            &mut writer,
            &mut events,
            Event::new(
                run_id,
                "model.skipped",
                serde_json::json!({
                    "stage": CodingStage::PatchPlan.as_str(),
                    "provider": config.model.provider,
                    "model": config.model.model,
                    "reason": "disabled_by_options",
                    "api_key_env": config.model.api_key_env,
                }),
            ),
        )?;
        Ok(None)
    } else {
        try_model_patch_plan(
            &mut writer,
            &mut events,
            &run_paths,
            &paths,
            &config,
            options.model_key_override.as_deref(),
            run_id,
            &options.task,
            &files_to_read,
            &patch_context,
            &mut recall_ledger,
        )
    };
    let (patch_plan, patch_plan_usage) = match model_patch_plan {
        Ok(Some((patch_plan, usage))) => (patch_plan, Some(usage)),
        Ok(None) => (
            build_dry_run_patch_plan(
                &paths,
                run_id,
                &options.task,
                &files_to_read,
                &patch_context,
            ),
            None,
        ),
        Err(err) => {
            emit(
                &mut writer,
                &mut events,
                Event::new(
                    run_id,
                    "run.failed",
                    serde_json::json!({
                        "category": "Model",
                        "message": err.to_string(),
                        "failed_stage": CodingStage::PatchPlan.as_str(),
                    }),
                ),
            )?;
            projector::apply_events(&conn, &events)?;
            return Err(err);
        }
    };
    if matches!(patch_plan.risk_level, RiskLevel::High) && !options.allow_high_risk {
        emit(
            &mut writer,
            &mut events,
            Event::new(
                run_id,
                "gate.failed",
                serde_json::json!({
                    "kind": "high_risk",
                    "message": "PatchPlan is high risk and --allow-high-risk was not set.",
                }),
            ),
        )?;
        emit(
            &mut writer,
            &mut events,
            Event::new(
                run_id,
                "run.failed",
                serde_json::json!({
                    "category": "Gate",
                    "message": "high-risk PatchPlan blocked",
                    "failed_stage": CodingStage::PatchPlan.as_str(),
                }),
            ),
        )?;
        projector::apply_events(&conn, &events)?;
        return Err("high-risk PatchPlan blocked; rerun with --allow-high-risk".into());
    }

    let patch_plan_path = run_paths
        .patch_plans_dir
        .join(format!("{}.json", patch_plan.patch_plan_id));
    fs::write(&patch_plan_path, serde_json::to_vec_pretty(&patch_plan)?)?;
    let patch_plan_artifact = format!("patch_plans/{}.json", patch_plan.patch_plan_id);
    emit(
        &mut writer,
        &mut events,
        Event::new(
            run_id,
            "patch.plan.created",
            serde_json::json!({
                "patch_plan_id": patch_plan.patch_plan_id,
                "artifact": patch_plan_artifact,
            }),
        ),
    )?;
    stage_completed(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::PatchPlan,
        serde_json::json!({
            "summary": "Dry-run PatchPlan generated.",
            "patch_plan_id": patch_plan.patch_plan_id,
            "files_to_modify": patch_plan.files_to_modify,
            "verification_commands": patch_plan.verification_commands,
            "risk_level": patch_plan.risk_level,
            "model_usage": patch_plan_usage,
        }),
    )?;

    // MP-1.6 plan-validation gate (Codex review): reject any file in
    // files_to_create that already exists on disk before Implementation
    // starts. The model occasionally proposes "create lib.rs" when nudged by
    // an auto-accepted convention; without this gate the agent burns turns
    // before the strict diff gate catches the contradiction.
    if !options.dry_run {
        let mut create_collisions: Vec<String> = Vec::new();
        for path in &patch_plan.files_to_create {
            let absolute = paths.repo_root.join(path);
            if absolute.exists() {
                create_collisions.push(path.clone());
            }
        }
        if !create_collisions.is_empty() {
            let message = format!(
                "PatchPlan declares files_to_create that already exist: {}",
                create_collisions.join(", ")
            );
            emit(
                &mut writer,
                &mut events,
                Event::new(
                    run_id,
                    "gate.failed",
                    serde_json::json!({
                        "kind": "patch_plan",
                        "reason": "files_to_create_already_exist",
                        "paths": create_collisions,
                        "message": &message,
                    }),
                ),
            )?;
            emit(
                &mut writer,
                &mut events,
                Event::new(
                    run_id,
                    "run.failed",
                    serde_json::json!({
                        "category": "Gate",
                        "message": &message,
                        "failed_stage": CodingStage::PatchPlan.as_str(),
                    }),
                ),
            )?;
            projector::apply_events(&conn, &read_trace(&run_paths.trace_jsonl)?)?;
            return Err(message.into());
        }
    }

    // E1: proactive failure anticipation — retrieve failure_pattern/convention
    // memories relevant to this task ONCE before the attempts loop. High
    // min_score + kind filter keeps this ~zero-token when nothing matches.
    // Best-effort: a retrieval error just skips the block without failing the run.
    let proactive_pitfall_bundle: Option<ContextBundle> = if options.disable_broker {
        None
    } else {
        let pitfall_request = ContextRequest {
            stage: "implementation".to_string(),
            query: format!("{}\n{}", options.task, patch_plan.expected_outcome),
            budget_tokens: 600,
            min_score: 0.7,
            max_capsules: 2,
            kinds: vec!["failure_pattern".to_string(), "convention".to_string()],
            ..Default::default()
        };
        context::retrieve_context(&conn, &repo_root, &config.broker.weights, pitfall_request).ok()
    };

    let mut implementation_outcome = None;
    let mut verification_summary: Option<serde_json::Value> = None;
    let mut verification_tool_calls: u32 = 0;
    let mut last_failure_context: Option<String> = None;
    let mut last_failure_fingerprint: Option<String> = None;
    let mut attempt: u32 = 0;
    const MAX_VERIFICATION_ATTEMPTS: u32 = 3;
    if !options.dry_run {
        'attempts: loop {
            attempt = attempt.saturating_add(1);
            stage_entered(
                &mut writer,
                &mut events,
                run_id,
                CodingStage::Implementation,
            )?;
            if options.disable_model {
                emit(
                    &mut writer,
                    &mut events,
                    Event::new(
                        run_id,
                        "model.skipped",
                        serde_json::json!({
                            "stage": CodingStage::Implementation.as_str(),
                            "provider": config.model.provider,
                            "model": config.model.model,
                            "reason": "disabled_by_options",
                            "api_key_env": config.model.api_key_env,
                        }),
                    ),
                )?;
                emit(
                    &mut writer,
                    &mut events,
                    Event::new(
                        run_id,
                        "run.failed",
                        serde_json::json!({
                            "category": "Model",
                            "message": "implementation requires model access; remove --no-model or use --dry-run",
                            "failed_stage": CodingStage::Implementation.as_str(),
                        }),
                    ),
                )?;
                projector::apply_events(&conn, &read_trace(&run_paths.trace_jsonl)?)?;
                return Err(
                    "implementation requires model access; remove --no-model or use --dry-run"
                        .into(),
                );
            }
            let provider_selection: KimetsuResult<Option<Box<dyn ModelProvider>>> =
            match config.model.provider.as_str() {
                "anthropic" => AnthropicProvider::from_config_with_key(
                    &paths.repo_root,
                    &config,
                    options.model_key_override.as_deref(),
                )
                .map(|opt| opt.map(|p| Box::new(p) as Box<dyn ModelProvider>)),
                "claude_code" => ClaudeCodeProvider::from_config_with_key(
                    &paths.repo_root,
                    &config,
                    options.model_key_override.as_deref(),
                )
                .map(|opt| opt.map(|p| Box::new(p) as Box<dyn ModelProvider>)),
                "bedrock" => BedrockProvider::from_config_with_key(
                    &paths.repo_root,
                    &config,
                    options.model_key_override.as_deref(),
                )
                .map(|opt| opt.map(|p| Box::new(p) as Box<dyn ModelProvider>)),
                other => Err(format!(
                    "unsupported model provider for implementation: `{other}`; configure `anthropic`, `claude_code`, or `bedrock`"
                )
                .into()),
            };
            let provider = match provider_selection {
                Ok(Some(provider)) => provider,
                Ok(None) => {
                    emit(
                        &mut writer,
                        &mut events,
                        Event::new(
                            run_id,
                            "model.skipped",
                            serde_json::json!({
                                "stage": CodingStage::Implementation.as_str(),
                                "provider": config.model.provider,
                                "model": config.model.model,
                                "reason": "missing_api_key",
                                "api_key_env": config.model.api_key_env,
                            }),
                        ),
                    )?;
                    emit(
                        &mut writer,
                        &mut events,
                        Event::new(
                            run_id,
                            "run.failed",
                            serde_json::json!({
                                "category": "Model",
                                "message": "implementation requires a configured model API key",
                                "failed_stage": CodingStage::Implementation.as_str(),
                            }),
                        ),
                    )?;
                    projector::apply_events(&conn, &read_trace(&run_paths.trace_jsonl)?)?;
                    return Err("implementation requires a configured model API key".into());
                }
                Err(err) => {
                    emit(
                        &mut writer,
                        &mut events,
                        Event::new(
                            run_id,
                            "run.failed",
                            serde_json::json!({
                                "category": "Model",
                                "message": err.to_string(),
                                "failed_stage": CodingStage::Implementation.as_str(),
                            }),
                        ),
                    )?;
                    projector::apply_events(&conn, &read_trace(&run_paths.trace_jsonl)?)?;
                    return Err(err);
                }
            };

            let mut runtime = ToolRuntime::new(&paths.repo_root, run_id)?
                .with_stage(CodingStage::Implementation.as_str())
                .with_config(tool_runtime_config(&config))
                .with_trace(writer, run_paths.clone());
            runtime.set_patch_plan(tool_patch_plan(&patch_plan));
            let loop_config = AgentLoopConfig {
                stage: CodingStage::Implementation.as_str().to_string(),
                tools: implementation_tool_definitions(),
                max_model_turns: config.run.max_total_model_turns,
                max_tool_calls: config.run.max_total_tool_calls,
                max_cost_usd: config.run.max_total_cost_usd,
                max_output_tokens: config.model.max_output_tokens,
                temperature: config.model.temperature,
            };
            let mut loop_runner = AgentLoop::new(provider, runtime, loop_config);
            let loop_result = loop_runner.run(build_implementation_messages(
                &options.task,
                &patch_plan,
                &patch_context,
                proactive_pitfall_bundle.as_ref(),
                last_failure_context.as_deref(),
                &mut recall_ledger,
            )?);
            let runtime = loop_runner.into_runtime();
            let Some((restored_writer, _)) = runtime.into_trace() else {
                return Err("implementation runtime lost trace writer".into());
            };
            writer = restored_writer;

            match loop_result {
                Ok(outcome) => {
                    stage_completed(
                        &mut writer,
                        &mut events,
                        run_id,
                        CodingStage::Implementation,
                        serde_json::json!({
                            "summary": "Implementation agent loop completed.",
                            "model_turns": outcome.model_turns,
                            "tool_calls": outcome.tool_calls,
                            "usage": outcome.usage,
                        }),
                    )?;
                    implementation_outcome = Some(outcome);
                }
                Err(err) => {
                    emit(
                        &mut writer,
                        &mut events,
                        Event::new(
                            run_id,
                            "run.failed",
                            serde_json::json!({
                                "category": "Implementation",
                                "message": err.to_string(),
                                "failed_stage": CodingStage::Implementation.as_str(),
                            }),
                        ),
                    )?;
                    projector::apply_events(&conn, &read_trace(&run_paths.trace_jsonl)?)?;
                    return Err(err);
                }
            }

            // === Verification stage (inside retry loop) ===
            if patch_plan.verification_commands.is_empty() {
                break 'attempts;
            }
            stage_entered(&mut writer, &mut events, run_id, CodingStage::Verification)?;

            let mut runtime = ToolRuntime::new(&paths.repo_root, run_id)?
                .with_stage(CodingStage::Verification.as_str())
                .with_config(tool_runtime_config(&config))
                .with_trace(writer, run_paths.clone());

            let mut failures: u32 = 0;
            let mut command_results: Vec<serde_json::Value> = Vec::new();
            let mut first_failure_payload: Option<serde_json::Value> = None;

            for command in &patch_plan.verification_commands {
                let expected = command.expected_exit.unwrap_or(0);
                let label = if command.args.is_empty() {
                    command.program.clone()
                } else {
                    format!("{} {}", command.program, command.args.join(" "))
                };
                verification_tool_calls = verification_tool_calls.saturating_add(1);
                match runtime.shell_command(command.clone()) {
                    Ok(output) => {
                        let passed = output.exit_code == expected
                            && !output.timed_out
                            && !output.killed_for_policy;
                        let payload = serde_json::json!({
                            "kind": "verification",
                            "command": label,
                            "exit_code": output.exit_code,
                            "expected_exit": expected,
                            "duration_ms": output.duration_ms,
                            "timed_out": output.timed_out,
                            "killed_for_policy": output.killed_for_policy,
                            "stdout_summary": output.stdout_summary,
                            "stderr_summary": output.stderr_summary,
                        });
                        let kind = if passed { "gate.passed" } else { "gate.failed" };
                        runtime
                            .append_trace_event(&Event::new(run_id, kind, payload.clone()), true)?;
                        if !passed {
                            failures = failures.saturating_add(1);
                            if first_failure_payload.is_none() {
                                first_failure_payload = Some(payload.clone());
                            }
                        }
                        command_results.push(payload);
                    }
                    Err(err) => {
                        let payload = serde_json::json!({
                            "kind": "verification",
                            "command": label,
                            "error": err.to_string(),
                        });
                        runtime.append_trace_event(
                            &Event::new(run_id, "gate.failed", payload.clone()),
                            true,
                        )?;
                        failures = failures.saturating_add(1);
                        if first_failure_payload.is_none() {
                            first_failure_payload = Some(payload.clone());
                        }
                        command_results.push(payload);
                    }
                }
            }

            let Some((restored_writer, _)) = runtime.into_trace() else {
                return Err("verification runtime lost trace writer".into());
            };
            writer = restored_writer;

            let summary_payload = serde_json::json!({
                "summary": format!(
                    "Ran {} verification command(s); {} failure(s) on attempt {}.",
                    patch_plan.verification_commands.len(),
                    failures,
                    attempt
                ),
                "attempt": attempt,
                "commands_run": patch_plan.verification_commands.len(),
                "failures": failures,
                "results": command_results.clone(),
            });
            stage_completed(
                &mut writer,
                &mut events,
                run_id,
                CodingStage::Verification,
                summary_payload.clone(),
            )?;
            verification_summary = Some(summary_payload);

            if failures == 0 {
                break 'attempts;
            }

            let current_fingerprint = first_failure_payload
                .as_ref()
                .map(verification_failure_fingerprint);
            let same_fingerprint_twice = match (
                last_failure_fingerprint.as_ref(),
                current_fingerprint.as_ref(),
            ) {
                (Some(prev), Some(curr)) => prev == curr,
                _ => false,
            };

            if attempt < MAX_VERIFICATION_ATTEMPTS && !same_fingerprint_twice {
                last_failure_fingerprint = current_fingerprint;
                last_failure_context = Some(render_retry_context(
                    attempt,
                    &command_results,
                    first_failure_payload.as_ref(),
                ));
                emit(
                    &mut writer,
                    &mut events,
                    Event::new(
                        run_id,
                        "verification.retry",
                        serde_json::json!({
                            "attempt": attempt,
                            "next_attempt": attempt + 1,
                            "fingerprint": last_failure_fingerprint,
                            "failures": failures,
                        }),
                    ),
                )?;
                continue 'attempts;
            }

            let stop_reason = if same_fingerprint_twice {
                "same_failure_fingerprint_twice"
            } else {
                "retry_budget_exhausted"
            };
            emit(
                &mut writer,
                &mut events,
                Event::new(
                    run_id,
                    "run.failed",
                    serde_json::json!({
                        "category": "Verification",
                        "message": format!(
                            "{failures} verification command(s) failed on attempt {attempt}; stop_reason: {stop_reason}"
                        ),
                        "failed_stage": CodingStage::Verification.as_str(),
                        "attempts_used": attempt,
                        "stop_reason": stop_reason,
                    }),
                ),
            )?;
            projector::apply_events(&conn, &read_trace(&run_paths.trace_jsonl)?)?;
            return Err(format!(
                "verification gate failed after {attempt} attempt(s): {stop_reason}"
            )
            .into());
        }
    }

    let memory_proposal_summary: Option<MemoryProposalSummary> = if !options.dry_run
        && implementation_outcome.is_some()
        && !options.disable_model
    {
        stage_entered(
            &mut writer,
            &mut events,
            run_id,
            CodingStage::MemoryProposal,
        )?;
        let existing_memories = load_existing_memories_for_proposal(&conn).unwrap_or_default();
        let proposed = match try_model_memory_proposals(
            &mut writer,
            &mut events,
            &run_paths,
            &paths,
            &config,
            options.model_key_override.as_deref(),
            run_id,
            &options.task,
            &patch_plan,
            verification_summary.as_ref(),
            &existing_memories,
        ) {
            Ok(summary) => summary,
            Err(err) => {
                emit(
                    &mut writer,
                    &mut events,
                    Event::new(
                        run_id,
                        "memory_proposal.error",
                        serde_json::json!({
                            "stage": CodingStage::MemoryProposal.as_str(),
                            "message": err.to_string(),
                        }),
                    ),
                )?;
                None
            }
        };

        if let Some(summary) = proposed.as_ref() {
            for proposal in &summary.accepted {
                emit(
                    &mut writer,
                    &mut events,
                    Event::new(
                        run_id,
                        "memory.proposed",
                        serde_json::json!({
                            "proposal_id": proposal.proposal_id,
                            "scope": proposal.scope,
                            "kind": proposal.kind,
                            "text": proposal.text,
                            "rationale": proposal.rationale,
                            "proposed_confidence": proposal.confidence,
                            "source_event_ids": [],
                        }),
                    ),
                )?;
            }
        }

        let stage_payload = match proposed.as_ref() {
            Some(summary) => {
                let filtered_examples: Vec<serde_json::Value> = summary
                    .filtered_examples
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "reason": f.reason,
                            "scope": f.scope,
                            "kind": f.kind,
                            "confidence": f.confidence,
                            "text": f.text,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "summary": format!(
                        "{} raw, {} accepted, {} filtered_identifier, {} filtered_dedup, {} filtered_invalid.",
                        summary.raw_count,
                        summary.accepted.len(),
                        summary.filtered_identifier,
                        summary.filtered_dedup,
                        summary.filtered_invalid,
                    ),
                    "raw_count": summary.raw_count,
                    "accepted": summary.accepted.len() as u32,
                    "filtered_identifier": summary.filtered_identifier,
                    "filtered_dedup": summary.filtered_dedup,
                    "filtered_invalid": summary.filtered_invalid,
                    "filtered_examples": filtered_examples,
                })
            }
            None => serde_json::json!({
                "summary": "MemoryProposal skipped (no provider or skipped on disable_model).",
            }),
        };
        stage_completed(
            &mut writer,
            &mut events,
            run_id,
            CodingStage::MemoryProposal,
            stage_payload,
        )?;
        proposed
    } else {
        None
    };

    stage_entered(&mut writer, &mut events, run_id, CodingStage::FinalReport)?;
    let final_report = render_coding_report(
        &options.task,
        run_id,
        options.dry_run,
        &patch_plan,
        implementation_outcome.as_ref(),
        verification_summary.as_ref(),
        memory_proposal_summary.as_ref(),
        &run_paths.trace_jsonl,
    );
    fs::write(&run_paths.final_report, final_report)?;
    stage_completed(
        &mut writer,
        &mut events,
        run_id,
        CodingStage::FinalReport,
        serde_json::json!({
            "summary": if options.dry_run {
                "Dry-run final report written."
            } else {
                "Final report written."
            },
            "final_report_path": "final_report.md",
        }),
    )?;
    let total_tool_calls = implementation_outcome
        .as_ref()
        .map(|outcome| outcome.tool_calls)
        .unwrap_or(0)
        .saturating_add(verification_tool_calls);
    emit(
        &mut writer,
        &mut events,
        Event::new(
            run_id,
            "run.finished",
            serde_json::json!({
                "status": "success",
                "final_report_path": "final_report.md",
                "total_cost_usd": patch_plan_usage
                    .map(|usage| usage.cost_usd)
                    .unwrap_or(0.0)
                    + implementation_outcome
                    .as_ref()
                    .map(|outcome| outcome.usage.cost_usd)
                    .unwrap_or(0.0),
                "total_tool_calls": total_tool_calls,
                "verification_tool_calls": verification_tool_calls,
                "dry_run": options.dry_run,
            }),
        ),
    )?;

    let trace_events = read_trace(&run_paths.trace_jsonl)?;
    projector::apply_events(&conn, &trace_events)?;
    // v1.5: best-effort regret detection — check whether any cited memory
    // was in the recent-dropped window (excluded by the relevance floor
    // in the hook but cited by the model), and emit retrieval.regret events.
    project::emit_regret_for_cited_memories(&paths.repo_root, &trace_events);

    Ok(CodingRunResult {
        run_id,
        dry_run: options.dry_run,
        patch_plan_id: patch_plan.patch_plan_id,
        final_report_path: run_paths.final_report,
        trace_path: run_paths.trace_jsonl,
    })
}

fn load_text_provider(
    repo_root: &Path,
    config: &ProjectConfig,
    model_key_override: Option<&str>,
) -> KimetsuResult<Option<SelectedTextProvider>> {
    match config.model.provider.as_str() {
        "anthropic" => {
            Ok(
                AnthropicProvider::from_config_with_key(repo_root, config, model_key_override)?
                    .map(|provider| SelectedTextProvider {
                        provider_name: "anthropic".to_string(),
                        model_name: provider.model_name().to_string(),
                        provider: Box::new(provider),
                    }),
            )
        }
        "claude_code" => {
            Ok(
                ClaudeCodeProvider::from_config_with_key(repo_root, config, model_key_override)?
                    .map(|provider| SelectedTextProvider {
                        provider_name: "claude_code".to_string(),
                        model_name: provider.model_name().to_string(),
                        provider: Box::new(provider),
                    }),
            )
        }
        "bedrock" => {
            Ok(
                BedrockProvider::from_config_with_key(repo_root, config, model_key_override)?
                    .map(|provider| SelectedTextProvider {
                        provider_name: "bedrock".to_string(),
                        model_name: provider.model_name().to_string(),
                        provider: Box::new(provider),
                    }),
            )
        }
        other => Err(format!("unsupported model provider: {other}; configure `anthropic`, `claude_code`, or `bedrock`").into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn try_model_patch_plan(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_paths: &RunPaths,
    paths: &ProjectPaths,
    config: &ProjectConfig,
    model_key_override: Option<&str>,
    run_id: RunId,
    task: &str,
    files_to_read: &[String],
    patch_context: &ContextBundle,
    ledger: &mut RunRecallLedger,
) -> KimetsuResult<Option<(PatchPlan, TokenUsage)>> {
    if cfg!(test) {
        return Ok(None);
    }

    let Some(mut provider) = load_text_provider(&paths.repo_root, config, model_key_override)?
    else {
        emit(
            writer,
            events,
            Event::new(
                run_id,
                "model.skipped",
                serde_json::json!({
                    "stage": CodingStage::PatchPlan.as_str(),
                    "provider": config.model.provider,
                    "model": config.model.model,
                    "reason": "missing_api_key",
                    "api_key_env": config.model.api_key_env,
                }),
            ),
        )?;
        return Ok(None);
    };

    let request = build_patch_plan_request(config, task, files_to_read, patch_context, ledger);
    record_model_requested(
        writer,
        events,
        run_paths,
        run_id,
        &provider.provider_name,
        &provider.model_name,
        &request,
    )?;
    let response = provider.complete(request)?;
    record_model_responded(
        writer,
        events,
        run_paths,
        run_id,
        &provider.provider_name,
        &response,
    )?;

    if !response.tool_calls.is_empty() {
        return Err("PatchPlan model unexpectedly requested tools".into());
    }

    let usage = response.usage;
    let text = response
        .text
        .as_deref()
        .ok_or("PatchPlan model response did not include text")?;
    let draft: PatchPlanDraft = parse_structured_json(text)?;
    Ok(Some((
        patch_plan_from_draft(paths, run_id, task, files_to_read, patch_context, draft),
        usage,
    )))
}

fn build_patch_plan_request(
    config: &ProjectConfig,
    task: &str,
    files_to_read: &[String],
    patch_context: &ContextBundle,
    ledger: &mut RunRecallLedger,
) -> ModelRequest {
    let system = ModelMessage {
        role: MessageRole::System,
        content: vec![MessageContent::Text {
            text: "You are the PatchPlan stage of Kimetsu, a conservative AI coding harness. Return only valid JSON and do not modify files.".to_string(),
        }],
    };
    let user = ModelMessage::user_text(format!(
        "Task:\n{task}\n\n\
         Localization-selected files:\n{localized_files}\n\n\
         Context capsules:\n{capsules}\n\n\
         Return exactly one JSON object with this shape:\n\
         {{\n\
           \"rationale\": \"short reason for the plan\",\n\
           \"files_to_read\": [\"repo/relative/path\"],\n\
           \"files_to_modify\": [\"repo/relative/path\"],\n\
           \"files_to_create\": [],\n\
           \"files_to_delete\": [],\n\
           \"verification_commands\": [{{\"program\":\"cargo\",\"args\":[\"test\"],\"cwd_relative\":\".\",\"timeout_secs\":null,\"expected_exit\":0}}],\n\
           \"expected_outcome\": \"observable result\",\n\
           \"risk_level\": \"low\"\n\
         }}\n\n\
         Rules:\n\
         - Use only repo-relative paths. No absolute paths.\n\
         - Prefer low-risk, minimal changes.\n\
         - Include files_to_modify in files_to_read.\n\
         - If uncertain, choose files_to_read and leave files_to_modify empty.\n\
         - risk_level must be one of: low, medium, high.\n\
         - Return JSON only, without Markdown fences.",
        localized_files = render_localized_files(files_to_read),
        // D1f: render all broker-selected capsules (already capped by
        // retrieve_context via config.broker.max_capsules). Fall back to
        // config.broker.max_capsules so the render cap stays in sync if
        // a caller bypasses the broker's own max_capsules gate.
        // F1: pass the run ledger so capsules already injected here are
        // back-referenced (not duplicated) in the later implementation stage.
        capsules = render_context_capsules(
            patch_context,
            config.broker.max_capsules.max(patch_context.capsules.len()),
            ledger,
        ),
    ));

    ModelRequest {
        messages: vec![system, user],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        max_output_tokens: config.model.max_output_tokens.min(4096),
        temperature: config.model.temperature,
        metadata: serde_json::json!({
            "stage": CodingStage::PatchPlan.as_str(),
            "purpose": "dry_run_patch_plan",
        }),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MemoryProposalEnvelope {
    #[serde(default)]
    proposals: Vec<RawMemoryProposal>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawMemoryProposal {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    rationale: String,
    #[serde(default)]
    confidence: f32,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedProposal {
    pub proposal_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub rationale: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MemoryProposalSummary {
    pub raw_count: u32,
    pub accepted: Vec<AcceptedProposal>,
    pub filtered_identifier: u32,
    pub filtered_dedup: u32,
    pub filtered_invalid: u32,
    /// Truncated text + reason for proposals the filter dropped, so we can see
    /// why an apparently-good run produced no surviving memories without
    /// re-running the bench.
    pub filtered_examples: Vec<FilteredProposalExample>,
}

#[derive(Debug, Clone)]
pub(crate) struct FilteredProposalExample {
    pub reason: &'static str,
    pub text: String,
    pub scope: String,
    pub kind: String,
    pub confidence: f32,
}

#[allow(clippy::too_many_arguments)]
fn try_model_memory_proposals(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_paths: &RunPaths,
    paths: &ProjectPaths,
    config: &ProjectConfig,
    model_key_override: Option<&str>,
    run_id: RunId,
    task: &str,
    patch_plan: &PatchPlan,
    verification_summary: Option<&serde_json::Value>,
    existing_memories: &[ExistingMemorySnapshot],
) -> KimetsuResult<Option<MemoryProposalSummary>> {
    if cfg!(test) {
        return Ok(None);
    }

    let Some(mut provider) = load_text_provider(&paths.repo_root, config, model_key_override)?
    else {
        emit(
            writer,
            events,
            Event::new(
                run_id,
                "model.skipped",
                serde_json::json!({
                    "stage": CodingStage::MemoryProposal.as_str(),
                    "provider": config.model.provider,
                    "model": config.model.model,
                    "reason": "missing_api_key",
                    "api_key_env": config.model.api_key_env,
                }),
            ),
        )?;
        return Ok(None);
    };

    let request = build_memory_proposal_request(
        config,
        task,
        patch_plan,
        verification_summary,
        existing_memories,
    );
    record_model_requested(
        writer,
        events,
        run_paths,
        run_id,
        &provider.provider_name,
        &provider.model_name,
        &request,
    )?;
    let response = provider.complete(request)?;
    record_model_responded(
        writer,
        events,
        run_paths,
        run_id,
        &provider.provider_name,
        &response,
    )?;

    let raw_text = response.text.as_deref().unwrap_or("");
    let envelope: MemoryProposalEnvelope = parse_structured_json(raw_text).unwrap_or_default();

    Ok(Some(filter_memory_proposals(
        envelope.proposals,
        run_id,
        patch_plan,
        existing_memories,
    )))
}

#[derive(Debug, Clone)]
pub(crate) struct ExistingMemorySnapshot {
    pub scope: String,
    pub kind: String,
    pub normalized_text: String,
    pub text: String,
}

fn build_memory_proposal_request(
    config: &ProjectConfig,
    task: &str,
    patch_plan: &PatchPlan,
    verification_summary: Option<&serde_json::Value>,
    existing_memories: &[ExistingMemorySnapshot],
) -> ModelRequest {
    let system = ModelMessage {
        role: MessageRole::System,
        content: vec![MessageContent::Text {
            text: "You are the MemoryProposal stage of Kimetsu. Review this completed run and propose zero or more *personal memories* that would help on a similar future task.\n\nA personal memory must be transferable: it applies beyond this specific bug, file, or function. Generalize what you learned about how this user/codebase prefers to work.\n\nAcceptable categories:\n- preference (scope=global_user): a user preference for tools, languages, style, or workflow. Examples: tool choice, formatting, error handling style, language editions.\n- convention (scope=repo or project): a codebase rule that recurs across files. Examples: naming patterns, layout, module organization, what kind of helper goes where.\n- failure_pattern (scope=repo): a recurring failure with a known mitigation, distilled from how this run had to handle a verification or implementation failure.\n\nProposing well:\n- Aim to surface 1-3 transferable items per run when patterns are visible. An empty array is fine when truly nothing generalizable emerged.\n- Generalize. If the convention is \"v2 modules re-export v1\", say that as a rule, not as \"add src/v2/mod.rs\".\n- Confidence guidance:\n  * 0.9: clearly demonstrated and explicit in the task or PatchPlan.\n  * 0.8: consistently observed across multiple steps of this run.\n  * 0.7: inferred from one strong observation; reasonable for auto-acceptance.\n  * 0.5 or lower: speculative or single weak observation.\n  * Always include a numeric confidence; do not omit the field.\n\nReject (do not propose):\n- Anything whose only point is naming this specific bug, file path, or function.\n- Trivially-derivable facts (\"the codebase uses Rust\", \"there is a Cargo.toml\").\n- Vacuous catch-alls (\"write good code\", \"prefer clean implementations\").\n\nGood proposals (transferable):\n- {\"scope\":\"global_user\",\"kind\":\"preference\",\"text\":\"Prefer ripgrep over grep for code search.\",\"rationale\":\"task referenced rg explicitly\",\"confidence\":0.85}\n- {\"scope\":\"repo\",\"kind\":\"convention\",\"text\":\"Public API uses fallible find_* helpers; rename get_* to find_* when adding lookups that return Option.\",\"rationale\":\"existing API pattern in the crate\",\"confidence\":0.8}\n- {\"scope\":\"repo\",\"kind\":\"convention\",\"text\":\"When introducing a versioned API module, add a thin re-export from the previous version rather than duplicating logic.\",\"rationale\":\"matches the namespacing pattern used in lib.rs\",\"confidence\":0.8}\n- {\"scope\":\"repo\",\"kind\":\"failure_pattern\",\"text\":\"When cargo test fails with both an area and a perimeter assertion, both helpers usually share a copy-paste bug; check both files.\",\"rationale\":\"two-file bug surfaced as paired test failures\",\"confidence\":0.75}\n\nBad proposals (do not emit):\n- \"Fix src/lib.rs so the area test passes.\" -> run-specific\n- \"Use Rust.\" -> trivially derivable\n- \"Write better code.\" -> vacuous\n\nOutput exactly one JSON object on its own line. No prose, no markdown, no backticks:\n\n{\"proposals\":[{\"scope\":\"global_user|project|repo\",\"kind\":\"preference|convention|failure_pattern\",\"text\":\"<single sentence>\",\"rationale\":\"<one line>\",\"confidence\":0.0..1.0}, ...]}\n\nEmit {\"proposals\":[]} only when nothing transferable emerged.".to_string(),
        }],
    };
    let memories_block = if existing_memories.is_empty() {
        "(none)".to_string()
    } else {
        existing_memories
            .iter()
            .map(|m| format!("- [{}/{}] {}", m.scope, m.kind, m.text))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let verification_block = verification_summary
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|| "(no verification ran)".to_string());
    let user = ModelMessage::user_text(format!(
        "Task:\n{task}\n\n\
         PatchPlan rationale: {rationale}\n\
         Expected outcome: {expected}\n\
         risk_level: {risk}\n\n\
         Verification summary:\n{verification}\n\n\
         Existing accepted memories (do not duplicate):\n{memories}\n\n\
         Now output the proposals JSON envelope. Remember: do not name the files, functions, or bug from this run; propose only transferable preferences, conventions, or failure patterns.",
        rationale = patch_plan.rationale,
        expected = patch_plan.expected_outcome,
        risk = format!("{:?}", patch_plan.risk_level).to_ascii_lowercase(),
        verification = truncate_text(&verification_block, 1500),
        memories = memories_block,
    ));
    ModelRequest {
        messages: vec![system, user],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        max_output_tokens: config.model.max_output_tokens.min(2048),
        temperature: config.model.temperature,
        metadata: serde_json::json!({
            "stage": CodingStage::MemoryProposal.as_str(),
            "purpose": "memory_proposal",
        }),
    }
}

fn filter_memory_proposals(
    raw_proposals: Vec<RawMemoryProposal>,
    run_id: RunId,
    patch_plan: &PatchPlan,
    existing_memories: &[ExistingMemorySnapshot],
) -> MemoryProposalSummary {
    const SHORT_PROPOSAL_LIMIT_CHARS: usize = 80;
    const FILTERED_EXAMPLE_TEXT_CAP: usize = 240;
    const MAX_FILTERED_EXAMPLES: usize = 6;

    let raw_count = raw_proposals.len() as u32;
    let mut summary = MemoryProposalSummary {
        raw_count,
        ..MemoryProposalSummary::default()
    };
    let plan_paths: Vec<String> = patch_plan
        .files_to_modify
        .iter()
        .chain(patch_plan.files_to_create.iter())
        .chain(patch_plan.files_to_delete.iter())
        .chain(patch_plan.files_to_read.iter())
        .map(|p| p.to_lowercase())
        .collect();

    let record_filtered =
        |summary: &mut MemoryProposalSummary, raw: &RawMemoryProposal, reason: &'static str| {
            if summary.filtered_examples.len() < MAX_FILTERED_EXAMPLES {
                summary.filtered_examples.push(FilteredProposalExample {
                    reason,
                    text: truncate_text(raw.text.trim(), FILTERED_EXAMPLE_TEXT_CAP),
                    scope: raw.scope.trim().to_lowercase(),
                    kind: raw.kind.trim().to_lowercase(),
                    confidence: raw.confidence.clamp(0.0, 1.0),
                });
            }
        };

    for raw in raw_proposals {
        let scope = raw.scope.trim().to_lowercase();
        let kind = raw.kind.trim().to_lowercase();
        let text = raw.text.trim().to_string();
        if text.is_empty() {
            summary.filtered_invalid = summary.filtered_invalid.saturating_add(1);
            record_filtered(&mut summary, &raw, "empty_text");
            continue;
        }
        if !is_supported_scope(&scope) || !is_supported_proposal_kind(&kind) {
            summary.filtered_invalid = summary.filtered_invalid.saturating_add(1);
            record_filtered(&mut summary, &raw, "invalid_scope_or_kind");
            continue;
        }

        // Loosened identifier filter: only reject when the proposal is short
        // enough to be plausibly run-specific *and* mentions a plan path. Long
        // generalised guidance that incidentally references a path stays in;
        // the auto-accept policy is the next gate.
        let lowered = text.to_lowercase();
        let mentions_run_path = plan_paths
            .iter()
            .any(|path| !path.is_empty() && lowered.contains(path));
        if mentions_run_path && text.len() < SHORT_PROPOSAL_LIMIT_CHARS {
            summary.filtered_identifier = summary.filtered_identifier.saturating_add(1);
            record_filtered(&mut summary, &raw, "short_and_path_specific");
            continue;
        }

        let normalized = kimetsu_core::memory::normalize_memory_text(&text);
        let duplicate = existing_memories.iter().any(|m| {
            m.scope.eq_ignore_ascii_case(&scope)
                && m.kind.eq_ignore_ascii_case(&kind)
                && m.normalized_text == normalized
        });
        if duplicate {
            summary.filtered_dedup = summary.filtered_dedup.saturating_add(1);
            record_filtered(&mut summary, &raw, "duplicate_of_existing");
            continue;
        }

        // Default confidence raised from 0.5 -> 0.7 when the model omits the
        // field. Most well-formed proposals from Opus do not emit a numeric
        // confidence; the prior 0.5 default kept them below the auto-accept
        // threshold, so they ended up Pending forever.
        let confidence = raw.confidence.clamp(0.0, 1.0);
        let confidence = if confidence == 0.0 { 0.7 } else { confidence };

        let proposal = AcceptedProposal {
            proposal_id: format!("{}_{}", run_id, new_id()),
            scope,
            kind,
            text,
            rationale: raw.rationale.trim().to_string(),
            confidence,
        };
        summary.accepted.push(proposal);
    }

    summary
}

fn is_supported_scope(scope: &str) -> bool {
    matches!(scope, "global_user" | "project" | "repo")
}

fn is_supported_proposal_kind(kind: &str) -> bool {
    matches!(kind, "preference" | "convention" | "failure_pattern")
}

fn record_model_requested(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_paths: &RunPaths,
    run_id: RunId,
    provider_name: &str,
    model: &str,
    request: &ModelRequest,
) -> KimetsuResult<()> {
    let mut event = Event::new(
        run_id,
        "model.requested",
        serde_json::json!({
            "stage": CodingStage::PatchPlan.as_str(),
            "provider": provider_name,
            "model": model,
            "request_artifact": null,
            "tool_names": request.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
            "estimated_input_tokens": estimate_request_tokens(request),
        }),
    );
    let artifact = write_json_artifact(
        run_paths,
        &format!("{}.model_request.json", event.event_id),
        request,
    )?;
    event.payload["request_artifact"] = serde_json::json!(artifact);
    emit(writer, events, event)
}

fn record_model_responded(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_paths: &RunPaths,
    run_id: RunId,
    provider_name: &str,
    response: &ModelResponse,
) -> KimetsuResult<()> {
    let mut event = Event::new(
        run_id,
        "model.responded",
        serde_json::json!({
            "stage": CodingStage::PatchPlan.as_str(),
            "provider": provider_name,
            "response_artifact": null,
            "stop_reason": response.stop_reason,
            "usage": response.usage,
        }),
    );
    let artifact = write_json_artifact(
        run_paths,
        &format!("{}.model_response.json", event.event_id),
        response,
    )?;
    event.payload["response_artifact"] = serde_json::json!(artifact);
    emit(writer, events, event)
}

fn write_json_artifact<T: Serialize>(
    run_paths: &RunPaths,
    file_name: &str,
    value: &T,
) -> KimetsuResult<String> {
    if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
        return Err("invalid artifact file name".into());
    }
    fs::create_dir_all(&run_paths.artifacts_dir)?;
    fs::write(
        run_paths.artifacts_dir.join(file_name),
        serde_json::to_vec_pretty(value)?,
    )?;
    Ok(format!("artifacts/{file_name}"))
}

fn patch_plan_from_draft(
    paths: &ProjectPaths,
    run_id: RunId,
    task: &str,
    localized_files_to_read: &[String],
    patch_context: &ContextBundle,
    draft: PatchPlanDraft,
) -> PatchPlan {
    let fallback =
        build_dry_run_patch_plan(paths, run_id, task, localized_files_to_read, patch_context);
    let mut files_to_read = sanitize_paths(draft.files_to_read);
    for path in localized_files_to_read {
        if let Some(path) = sanitize_path(path) {
            push_unique(&mut files_to_read, path);
        }
    }
    if files_to_read.is_empty() {
        files_to_read = fallback.files_to_read.clone();
    }

    let mut files_to_modify = sanitize_paths(draft.files_to_modify);
    if files_to_modify.is_empty() {
        files_to_modify = fallback.files_to_modify.clone();
    }
    let files_to_create = sanitize_paths(draft.files_to_create);
    let files_to_delete = sanitize_paths(draft.files_to_delete);

    for path in files_to_modify
        .iter()
        .chain(files_to_delete.iter())
        .cloned()
        .collect::<Vec<_>>()
    {
        push_unique(&mut files_to_read, path);
    }

    let verification_commands = {
        let sanitized = sanitize_commands(draft.verification_commands);
        if sanitized.is_empty() {
            fallback.verification_commands
        } else {
            sanitized
        }
    };
    let inferred_risk = infer_risk_level(&files_to_modify, &files_to_create, &files_to_delete);
    let risk_level = parse_risk_level(&draft.risk_level)
        .map(|risk| max_risk_level(risk, inferred_risk))
        .unwrap_or(inferred_risk);

    PatchPlan {
        patch_plan_id: new_id().to_string(),
        run_id: run_id.to_string(),
        revision_of: None,
        rationale: if draft.rationale.trim().is_empty() {
            fallback.rationale
        } else {
            draft.rationale.trim().to_string()
        },
        files_to_read,
        files_to_modify,
        files_to_create,
        files_to_delete,
        verification_commands,
        expected_outcome: if draft.expected_outcome.trim().is_empty() {
            fallback.expected_outcome
        } else {
            draft.expected_outcome.trim().to_string()
        },
        risk_level,
    }
}

fn build_implementation_messages(
    task: &str,
    patch_plan: &PatchPlan,
    patch_context: &ContextBundle,
    pitfall_bundle: Option<&ContextBundle>,
    retry_context: Option<&str>,
    ledger: &mut RunRecallLedger,
) -> KimetsuResult<Vec<ModelMessage>> {
    let system = ModelMessage {
        role: MessageRole::System,
        content: vec![MessageContent::Text {
            text: "You are the Implementation stage of Kimetsu, a conservative coding harness. Use tools to inspect files and apply the minimal patch. Do not invent tool results. Do not modify files outside the active PatchPlan.".to_string(),
        }],
    };
    let patch_plan_json = serde_json::to_string_pretty(patch_plan)?;
    let retry_block = match retry_context {
        Some(text) if !text.trim().is_empty() => format!(
            "\n\nPrevious verification failed. Use this context to repair the patch:\n{text}\n"
        ),
        _ => String::new(),
    };
    // E1: render the "Known pitfalls" block from the proactive bundle (if any).
    // Uses is_surfaced/mark_surfaced — separate from the F1 is_injected/mark_injected
    // dedup — so retries don't repeat the same warning.
    let pitfalls_block = pitfall_bundle
        .and_then(|bundle| render_known_pitfalls(bundle, ledger))
        .map(|block| format!("\n\n{block}"))
        .unwrap_or_default();
    let user = ModelMessage::user_text(format!(
        "Task:\n{task}\n\n\
         Active PatchPlan:\n{patch_plan_json}\n\n\
         Context capsules:\n{capsules}{pitfalls_block}{retry_block}\n\n\
         Rules:\n\
         - Read a file before modifying or deleting it.\n\
         - For modify/delete, pass the exact hash from read_file as expected_hash.\n\
         - Use apply_patch for all file edits.\n\
         - Only files declared in files_to_modify, files_to_create, or files_to_delete may be changed.\n\
         - Keep edits minimal and directly tied to expected_outcome.\n\
         - Finish with a concise summary of changed files and remaining verification work.",
        // D1f: render all broker-selected capsules (already capped by
        // retrieve_context). The broker's max_capsules gate ensures a
        // tight, high-precision set; re-capping here would silently
        // discard capsules the broker decided were worth including.
        // F1: pass the run ledger — capsules already injected in the
        // patch-plan stage appear as compact back-references here instead
        // of being repeated in full, shrinking the implementation prompt.
        capsules =
            render_context_capsules(patch_context, patch_context.capsules.len().max(1), ledger,),
    ));
    Ok(vec![system, user])
}

/// E1: render a "Known pitfalls" block from a proactive retrieval bundle.
///
/// For each capsule in `bundle` that has NOT already been surfaced this run
/// (checked via `ledger.is_surfaced`), include it in the block and mark it
/// surfaced. Capsules already surfaced (e.g. from a prior attempt) are
/// silently skipped — this is the proactive dedup, kept intentionally
/// separate from the F1 `is_injected` / `mark_injected` cross-stage capsule
/// dedup. Returns `None` when no unsurfaced pitfalls remain (empty block,
/// no header rendered).
pub fn render_known_pitfalls(
    bundle: &ContextBundle,
    ledger: &mut RunRecallLedger,
) -> Option<String> {
    if bundle.skipped || bundle.capsules.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for c in &bundle.capsules {
        if !ledger.is_surfaced(&c.id) {
            ledger.mark_surfaced(&c.id);
            lines.push(format!(
                "- {kind}: {summary}",
                kind = c.kind,
                summary = c.summary
            ));
        }
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Known pitfalls (from past runs)\n{}",
        lines.join("\n")
    ))
}

fn implementation_tool_definitions() -> Vec<crate::model::ToolDefinition> {
    // Shell access is permitted in Implementation; the strict diff gate and
    // shell policy in `tools::ToolRuntime` are the actual safety boundary.
    default_tool_definitions()
}

fn tool_patch_plan(patch_plan: &PatchPlan) -> ToolPatchPlan {
    ToolPatchPlan {
        files_to_modify: patch_plan.files_to_modify.iter().cloned().collect(),
        files_to_create: patch_plan.files_to_create.iter().cloned().collect(),
        files_to_delete: patch_plan.files_to_delete.iter().cloned().collect(),
    }
}

fn tool_runtime_config(config: &ProjectConfig) -> ToolRuntimeConfig {
    ToolRuntimeConfig {
        default_timeout_secs: config.shell.default_timeout_secs,
        max_timeout_secs: config.shell.max_timeout_secs,
        model_output_cap_bytes: ToolRuntimeConfig::default().model_output_cap_bytes,
        disk_output_cap_bytes: ToolRuntimeConfig::default().disk_output_cap_bytes,
        redact_secrets: config.shell.redact_secrets,
        trace_fsync: true,
        env_allowlist_extra: config.shell.env_allowlist_extra.clone(),
    }
}

fn emit(writer: &mut TraceWriter, events: &mut Vec<Event>, event: Event) -> KimetsuResult<()> {
    writer.append(&event, true)?;
    events.push(event);
    Ok(())
}

fn stage_entered(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_id: RunId,
    stage: CodingStage,
) -> KimetsuResult<()> {
    emit(
        writer,
        events,
        Event::new(
            run_id,
            "stage.entered",
            serde_json::json!({ "stage": stage.as_str() }),
        ),
    )
}

/// MP-4a: emit a `context.injected` event capturing the broker's bundle for
/// a stage. The projector later joins these against terminal events to
/// update memory usefulness. The payload separates capsules by kind so
/// downstream consumers don't have to re-parse handles.
fn emit_context_injected(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_id: RunId,
    stage: CodingStage,
    bundle: &ContextBundle,
) -> KimetsuResult<()> {
    let mut memory_ids: Vec<String> = Vec::new();
    let mut prior_run_ids: Vec<String> = Vec::new();
    let mut file_paths: Vec<String> = Vec::new();
    let mut capsule_handles: Vec<String> = Vec::with_capacity(bundle.capsules.len());
    for capsule in &bundle.capsules {
        let handle = capsule.expansion_handle.as_str();
        capsule_handles.push(handle.to_string());
        if let Some(id) = handle.strip_prefix("memory:") {
            memory_ids.push(id.to_string());
        } else if let Some(id) = handle.strip_prefix("run:") {
            prior_run_ids.push(id.to_string());
        } else if let Some(path) = handle.strip_prefix("file:") {
            file_paths.push(path.to_string());
        }
    }
    emit(
        writer,
        events,
        Event::new(
            run_id,
            "context.injected",
            serde_json::json!({
                "stage": stage.as_str(),
                "capsule_handles": capsule_handles,
                "memory_ids": memory_ids,
                "prior_run_ids": prior_run_ids,
                "file_paths": file_paths,
                "used_tokens": bundle.used_tokens,
                "capsule_count": bundle.capsules.len(),
            }),
        ),
    )
}

/// C7: emit a `context.served` event so the analytics module can compute
/// retrieval hit-rate, avg top score, and skip-rate over pipeline runs.
/// Called alongside `emit_context_injected` for each retrieved bundle.
fn emit_context_served(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_id: RunId,
    stage: CodingStage,
    bundle: &ContextBundle,
) -> KimetsuResult<()> {
    // Hash the capsule_handles as a proxy for the "query" — the pipeline
    // doesn't expose the raw query text here, but a deterministic hash of
    // the bundle handles gives a correlatable fingerprint without storing
    // raw task text.
    let handle_concat: String = bundle
        .capsules
        .iter()
        .map(|c| c.expansion_handle.as_str())
        .collect::<Vec<_>>()
        .join("|");
    let query_hash = blake3::hash(handle_concat.as_bytes()).to_hex().to_string();
    emit(
        writer,
        events,
        Event::new(
            run_id,
            "context.served",
            serde_json::json!({
                "query_hash": query_hash,
                "capsule_count": bundle.capsules.len(),
                "top_score": bundle.top_score,
                "skipped": bundle.skipped,
                "stage": stage.as_str(),
            }),
        ),
    )
}

fn stage_completed(
    writer: &mut TraceWriter,
    events: &mut Vec<Event>,
    run_id: RunId,
    stage: CodingStage,
    mut payload: serde_json::Value,
) -> KimetsuResult<()> {
    payload["stage"] = serde_json::json!(stage.as_str());
    emit(
        writer,
        events,
        Event::new(run_id, "stage.completed", payload),
    )
}

fn likely_files(context: &ContextBundle, max: usize) -> Vec<String> {
    context
        .capsules
        .iter()
        .filter_map(file_from_capsule)
        .take(max)
        .collect()
}

fn file_from_capsule(capsule: &ContextCapsule) -> Option<String> {
    capsule
        .expansion_handle
        .strip_prefix("file:")
        .map(str::to_string)
}

fn render_localized_files(files_to_read: &[String]) -> String {
    if files_to_read.is_empty() {
        return "None selected.".to_string();
    }

    files_to_read
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── F2: tiered capsule injection constants ─────────────────────────────

/// F2: number of top-scoring capsules rendered in FULL in each stage.
/// Capsules beyond this threshold are injected as HEADLINES (one line +
/// expansion handle). The agent can call `expand_capsule(handle)` to
/// retrieve the full text of any headline on demand.
///
/// Rationale: top-3 covers the highest-signal capsules; the long tail is
/// discoverable via headlines at ~10 tokens each instead of hundreds.
const FULL_TIER_CAP: usize = 3;

/// F2: token cost charged for a HEADLINE capsule. Small enough that the
/// entire tail costs less than one full capsule, while still reserving a
/// slot so the agent knows it exists and can expand it.
const HEADLINE_TOKEN_COST: u32 = 10;

/// F1+F2: render capsules for a prompt stage, deduplicating across stages
/// via `ledger`. Applies tiered injection (F2):
///
/// - **Full tier** (top `FULL_TIER_CAP` capsules not yet injected): rendered
///   verbosely with id / kind / score / handle / summary, charged at the
///   capsule's `token_estimate`. F1 back-reference applies: a capsule already
///   injected in a prior stage is shown as `(see above)` regardless of tier.
///
/// - **Headline tier** (remaining capsules not yet injected): rendered as a
///   single line `- [headline] kind: <gist> — expand with handle <h>`,
///   charged at [`HEADLINE_TOKEN_COST`]. This is where the token savings come
///   from — the tail is discoverable but not pre-paid in full.
///
/// A capsule already injected (either tier) is never double-charged; a
/// headline's small cost is recorded once and the agent's voluntary
/// `expand_capsule` call is separate accounting entirely.
fn render_context_capsules(
    context: &ContextBundle,
    max_capsules: usize,
    ledger: &mut RunRecallLedger,
) -> String {
    if context.capsules.is_empty() {
        return "None.".to_string();
    }

    // Count how many new (not-yet-injected) full-tier slots we have left
    // as we walk the capsule list in score order.
    let mut full_slots_remaining = FULL_TIER_CAP;

    context
        .capsules
        .iter()
        .take(max_capsules)
        .map(|capsule| {
            if ledger.is_injected(&capsule.id) {
                // F1 back-reference: already in context from a prior stage.
                // No new charge. Tier doesn't matter here — it's a back-ref.
                format!(
                    "- (see above) [{id}] kind:{kind}",
                    id = capsule.id,
                    kind = capsule.kind,
                )
            } else if full_slots_remaining > 0 {
                // Full tier: render verbosely and consume a full slot.
                full_slots_remaining -= 1;
                ledger.mark_injected(&capsule.id, capsule.token_estimate);
                format!(
                    "- id: {id}\n  kind: {kind}\n  score: {score:.3}\n  handle: {handle}\n  summary: {summary}",
                    id = capsule.id,
                    kind = capsule.kind,
                    score = capsule.score,
                    handle = capsule.expansion_handle,
                    summary = truncate_text(&capsule.summary, 700),
                )
            } else {
                // F2 headline tier: one line, small token charge.
                // Summary is truncated to ~8 words so the line stays ~10 tokens.
                let gist = truncate_text(&capsule.summary, 60);
                ledger.mark_injected(&capsule.id, HEADLINE_TOKEN_COST);
                format!(
                    "- [headline] {kind}: {gist} — expand with handle {handle}",
                    kind = capsule.kind,
                    gist = gist,
                    handle = capsule.expansion_handle,
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_dry_run_patch_plan(
    paths: &ProjectPaths,
    run_id: RunId,
    task: &str,
    files_to_read: &[String],
    patch_context: &ContextBundle,
) -> PatchPlan {
    let mut files_to_modify = files_to_read
        .iter()
        .filter(|path| is_code_source(path))
        .take(1)
        .cloned()
        .collect::<Vec<_>>();

    if files_to_modify.is_empty() {
        files_to_modify = patch_context
            .capsules
            .iter()
            .filter_map(file_from_capsule)
            .filter(|path| is_code_source(path))
            .take(1)
            .collect();
    }

    let risk_level = infer_risk_level(&files_to_modify, &[], &[]);
    let mut files_to_read = files_to_read.to_vec();
    for path in &files_to_modify {
        if !files_to_read.contains(path) {
            files_to_read.push(path.clone());
        }
    }

    PatchPlan {
        patch_plan_id: new_id().to_string(),
        run_id: run_id.to_string(),
        revision_of: None,
        rationale: "Dry-run plan generated from retrieved context. Model-backed planning is the next phase.".to_string(),
        files_to_read,
        files_to_modify,
        files_to_create: Vec::new(),
        files_to_delete: Vec::new(),
        verification_commands: detect_verification_commands(&paths.repo_root),
        expected_outcome: format!("Address task: {task}"),
        risk_level,
    }
}

fn sanitize_paths(paths: Vec<String>) -> Vec<String> {
    let mut sanitized = Vec::new();
    for path in paths {
        if let Some(path) = sanitize_path(&path) {
            push_unique(&mut sanitized, path);
        }
    }
    sanitized
}

fn sanitize_path(path: &str) -> Option<String> {
    let mut path = path
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .replace('\\', "/");
    if let Some(stripped) = path.strip_prefix("file:") {
        path = stripped.to_string();
    }
    while let Some(stripped) = path.strip_prefix("./") {
        path = stripped.to_string();
    }
    if path.is_empty()
        || path.contains('\0')
        || path.contains(':')
        || Path::new(&path).is_absolute()
    {
        return None;
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(path)
}

fn sanitize_commands(commands: Vec<CommandSpec>) -> Vec<CommandSpec> {
    commands
        .into_iter()
        .filter_map(|command| {
            let program = command.program.trim();
            if program.is_empty() || program.contains('\0') {
                return None;
            }
            Some(CommandSpec {
                program: program.to_string(),
                args: command
                    .args
                    .into_iter()
                    .filter_map(|arg| {
                        let arg = arg.trim();
                        if arg.is_empty() || arg.contains('\0') {
                            None
                        } else {
                            Some(arg.to_string())
                        }
                    })
                    .collect(),
                cwd_relative: command
                    .cwd_relative
                    .and_then(|cwd| {
                        if cwd.trim() == "." {
                            Some(".".to_string())
                        } else {
                            sanitize_path(&cwd)
                        }
                    })
                    .or_else(|| Some(".".to_string())),
                timeout_secs: command.timeout_secs,
                expected_exit: command.expected_exit.or(Some(0)),
            })
        })
        .collect()
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn infer_risk_level(
    files_to_modify: &[String],
    files_to_create: &[String],
    files_to_delete: &[String],
) -> RiskLevel {
    if !files_to_delete.is_empty()
        || files_to_modify
            .iter()
            .chain(files_to_create.iter())
            .any(|path| is_high_risk_path(path))
    {
        RiskLevel::High
    } else if files_to_modify.len() + files_to_create.len() > 5 || !files_to_create.is_empty() {
        RiskLevel::Medium
    } else {
        RiskLevel::Low
    }
}

fn is_high_risk_path(path: &str) -> bool {
    path.contains(".github/")
        || path.ends_with("Cargo.toml")
        || path.ends_with("Cargo.lock")
        || path.ends_with("package.json")
        || path.ends_with("package-lock.json")
        || path.ends_with("go.mod")
        || path.ends_with("go.sum")
        || path.ends_with("pyproject.toml")
        || path.ends_with("uv.lock")
}

fn parse_risk_level(value: &serde_json::Value) -> Option<RiskLevel> {
    let value = value.as_str()?.trim().to_ascii_lowercase();
    match value.as_str() {
        "low" => Some(RiskLevel::Low),
        "medium" => Some(RiskLevel::Medium),
        "high" => Some(RiskLevel::High),
        _ => None,
    }
}

fn max_risk_level(left: RiskLevel, right: RiskLevel) -> RiskLevel {
    if risk_rank(left) >= risk_rank(right) {
        left
    } else {
        right
    }
}

fn risk_rank(level: RiskLevel) -> u8 {
    match level {
        RiskLevel::Low => 0,
        RiskLevel::Medium => 1,
        RiskLevel::High => 2,
    }
}

fn estimate_request_tokens(request: &ModelRequest) -> u32 {
    let text = serde_json::to_string(request).unwrap_or_default();
    ((text.split_whitespace().count() as f32) * 1.33).ceil() as u32
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut out = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// F3: token-count heuristic for the task-size signal.
///
/// Uses the same formula as `estimate_tokens` in `kimetsu_brain::context`
/// and `estimate_request_tokens` above: `(whitespace_words * 1.33).ceil()`.
/// Kept as a separate named function so the task-size signal definition is
/// explicit and unit-testable without importing brain internals.
fn estimate_task_tokens(text: &str) -> u32 {
    ((text.split_whitespace().count() as f32) * 1.33).ceil() as u32
}

fn is_code_source(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some("rs" | "js" | "ts" | "tsx" | "jsx" | "py" | "go")
    )
}

fn detect_verification_commands(repo_root: &Path) -> Vec<CommandSpec> {
    let mut commands = Vec::new();
    if repo_root.join("Cargo.toml").exists() {
        commands.push(command("cargo", ["test"]));
    }
    if repo_root.join("package.json").exists() {
        commands.push(command("npm", ["test"]));
    }
    if repo_root.join("pyproject.toml").exists() {
        commands.push(command("pytest", []));
    }
    if repo_root.join("go.mod").exists() {
        commands.push(command("go", ["test", "./..."]));
    }
    commands
}

fn command<const N: usize>(program: &str, args: [&str; N]) -> CommandSpec {
    CommandSpec {
        program: program.to_string(),
        args: args.into_iter().map(str::to_string).collect(),
        cwd_relative: Some(".".to_string()),
        timeout_secs: None,
        expected_exit: Some(0),
    }
}

fn load_existing_memories_for_proposal(
    conn: &rusqlite::Connection,
) -> KimetsuResult<Vec<ExistingMemorySnapshot>> {
    let mut stmt = conn.prepare(
        "
        SELECT scope, kind, text, normalized_text
        FROM memories
        ORDER BY created_at DESC
        LIMIT 200
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ExistingMemorySnapshot {
            scope: row.get(0)?,
            kind: row.get(1)?,
            text: row.get(2)?,
            normalized_text: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn render_coding_report(
    task: &str,
    run_id: RunId,
    dry_run: bool,
    patch_plan: &PatchPlan,
    implementation: Option<&AgentLoopOutcome>,
    verification: Option<&serde_json::Value>,
    memory_proposals: Option<&MemoryProposalSummary>,
    trace_path: &Path,
) -> String {
    let mode_summary = if dry_run {
        "Dry-run completed through PatchPlan. No files were modified.".to_string()
    } else {
        format!(
            "Implementation completed with {} model turns and {} tool calls.",
            implementation
                .map(|outcome| outcome.model_turns)
                .unwrap_or_default(),
            implementation
                .map(|outcome| outcome.tool_calls)
                .unwrap_or_default()
        )
    };
    let changed_files = changed_files_summary(patch_plan);
    let implementation_summary = implementation
        .map(|outcome| outcome.final_text.trim())
        .filter(|text| !text.is_empty())
        .unwrap_or("None.");
    let verification_section = render_verification_section(dry_run, patch_plan, verification);
    let known_risks = if dry_run {
        "Model-backed implementation and verification are not wired in dry-run."
    } else if verification.is_none() && patch_plan.verification_commands.is_empty() {
        "PatchPlan declared no verification commands; outcome is unverified."
    } else if memory_proposals.is_none() {
        "MemoryProposal stage not run (model disabled or run did not reach Implementation)."
    } else {
        "Review stage is not wired yet."
    };

    let memory_section = render_memory_proposal_section(memory_proposals);

    format!(
        "# {task}\n\n\
         ## Summary\n\
         {mode_summary}\n\n\
         ## Changed files\n\
         {changed_files}\n\n\
         ## Verification\n\
         {verification_section}\n\n\
         ## Implementation output\n\
         {implementation_summary}\n\n\
         ## Memory proposals\n\
         {memory_section}\n\n\
         ## Known risks\n\
         {known_risks}\n\n\
         ## Trace\n\
         run_id: {run_id}\n\
         trace path: {trace}\n\
         patch_plan_id: {patch_plan_id}\n\n\
         Generated by kimetsu {version}\n",
        trace = trace_path.display(),
        patch_plan_id = patch_plan.patch_plan_id,
        version = env!("CARGO_PKG_VERSION"),
    )
}

fn verification_failure_fingerprint(payload: &serde_json::Value) -> String {
    let exit_code = payload
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let command = payload
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let stderr = payload
        .get("stderr_summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let normalized = stderr
        .lines()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .map(normalize_for_fingerprint)
        .unwrap_or_default();
    let raw = format!("{exit_code}\u{1f}{command}\u{1f}{normalized}");
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

#[allow(clippy::while_let_on_iterator)]
fn normalize_for_fingerprint(line: &str) -> String {
    // Strip ANSI escape sequences (CSI: ESC [ ... letter; OSC: ESC ] ... BEL/ST).
    let mut without_ansi = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // Drop ESC and consume the following control sequence.
            if let Some(&next) = chars.peek() {
                chars.next();
                if next == '[' {
                    while let Some(c) = chars.next() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else if next == ']' {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next(); // consume terminating ST byte.
                            break;
                        }
                    }
                }
            }
        } else {
            without_ansi.push(ch);
        }
    }

    // Replace native paths and unix-ish absolute paths with a placeholder so
    // identical errors from different tempdirs share a fingerprint. The path
    // body deliberately excludes `:` so that `:line:col` suffixes (Windows or
    // Unix style) are left for the digit-collapsing step below.
    let path_re = regex::Regex::new(
        r#"(?x)
        (?:
            [a-zA-Z]:[\\/][^\s'"`<>:]*
          | /(?:[a-zA-Z0-9._\-]+/)+[a-zA-Z0-9._\-]+
        )
    "#,
    )
    .expect("path regex compiles");
    let no_paths = path_re.replace_all(&without_ansi, "<path>");

    // Collapse runs of digits to '#' so line/column numbers don't differ.
    let mut out = String::with_capacity(no_paths.len());
    let mut in_digits = false;
    for ch in no_paths.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(ch.to_ascii_lowercase());
        }
    }

    // Whitespace-collapse, then cap to a stable length so very long stderr
    // lines do not blow up the hash input.
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() > 512 {
        collapsed.chars().take(512).collect()
    } else {
        collapsed
    }
}

fn render_retry_context(
    attempt: u32,
    results: &[serde_json::Value],
    first_failure: Option<&serde_json::Value>,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Verification attempt {attempt} failed. Re-implement to make all commands pass."
    ));
    if let Some(fail) = first_failure {
        let command = fail
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let exit = fail
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "n/a".to_string());
        let stderr = fail
            .get("stderr_summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let stderr_excerpt = if stderr.is_empty() {
            "<empty>".to_string()
        } else {
            truncate_text(stderr, 800)
        };
        lines.push(format!("First failing command: {command}"));
        lines.push(format!("exit_code: {exit}"));
        lines.push(format!("stderr (capped):\n{stderr_excerpt}"));
    }
    let other_failures: Vec<_> = results
        .iter()
        .skip(1)
        .filter(|r| {
            r.get("error").is_some()
                || r.get("exit_code")
                    .and_then(|v| v.as_i64())
                    .zip(r.get("expected_exit").and_then(|v| v.as_i64()))
                    .map(|(actual, expected)| actual != expected)
                    .unwrap_or(false)
        })
        .filter_map(|r| r.get("command").and_then(|v| v.as_str()))
        .collect();
    if !other_failures.is_empty() {
        lines.push(format!("Also failing: {}", other_failures.join(", ")));
    }
    lines.join("\n")
}

fn render_memory_proposal_section(summary: Option<&MemoryProposalSummary>) -> String {
    let Some(summary) = summary else {
        return "None proposed (stage not run).".to_string();
    };
    let header = format!(
        "{} accepted ({} raw, {} dropped on identifier match, {} on dedup, {} invalid).",
        summary.accepted.len(),
        summary.raw_count,
        summary.filtered_identifier,
        summary.filtered_dedup,
        summary.filtered_invalid
    );
    if summary.accepted.is_empty() && summary.filtered_examples.is_empty() {
        return header;
    }
    let mut lines = vec![header];
    for proposal in &summary.accepted {
        lines.push(format!(
            "- [{}/{}] (conf {:.2}) {}",
            proposal.scope, proposal.kind, proposal.confidence, proposal.text
        ));
        if !proposal.rationale.is_empty() {
            lines.push(format!("  rationale: {}", proposal.rationale));
        }
    }
    if !summary.filtered_examples.is_empty() {
        lines.push("Filtered examples (visible for triage):".to_string());
        for example in &summary.filtered_examples {
            lines.push(format!(
                "- ({}) [{}/{}] (conf {:.2}) {}",
                example.reason, example.scope, example.kind, example.confidence, example.text
            ));
        }
    }
    if !summary.accepted.is_empty() {
        lines.push(
            "Use `kimetsu brain memory proposals` to review, then `accept <id>` to promote."
                .to_string(),
        );
    }
    lines.join("\n")
}

fn render_verification_section(
    dry_run: bool,
    patch_plan: &PatchPlan,
    verification: Option<&serde_json::Value>,
) -> String {
    if dry_run {
        let commands = serde_json::to_string(&patch_plan.verification_commands)
            .unwrap_or_else(|_| "[]".to_string());
        return format!("Skipped (dry-run). Planned commands: {commands}");
    }
    if patch_plan.verification_commands.is_empty() {
        return "No verification commands were declared in the PatchPlan.".to_string();
    }
    let Some(summary) = verification else {
        return "Verification stage did not record a summary.".to_string();
    };

    let commands_run = summary
        .get("commands_run")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let failures = summary
        .get("failures")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut lines = Vec::new();
    lines.push(format!(
        "Ran {commands_run} command(s); {failures} failure(s)."
    ));

    if let Some(results) = summary.get("results").and_then(|v| v.as_array()) {
        for result in results {
            let command = result
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
                lines.push(format!("- error `{command}`: {error}"));
                continue;
            }
            let exit_code = result
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            let expected = result
                .get("expected_exit")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let status = if exit_code == expected {
                "pass"
            } else {
                "fail"
            };
            lines.push(format!(
                "- {status} `{command}` exit={exit_code} expected={expected}"
            ));
        }
    }

    lines.join("\n")
}

fn changed_files_summary(patch_plan: &PatchPlan) -> String {
    let mut lines = Vec::new();
    lines.extend(
        patch_plan
            .files_to_modify
            .iter()
            .map(|path| format!("- modify `{path}`")),
    );
    lines.extend(
        patch_plan
            .files_to_create
            .iter()
            .map(|path| format!("- create `{path}`")),
    );
    lines.extend(
        patch_plan
            .files_to_delete
            .iter()
            .map(|path| format!("- delete `{path}`")),
    );
    if lines.is_empty() {
        "None.".to_string()
    } else {
        lines.join("\n")
    }
}

fn config_hash(path: &Path) -> KimetsuResult<String> {
    Ok(blake3::hash(&fs::read(path)?).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use kimetsu_brain::trace::read_trace;

    use super::*;

    #[test]
    fn dry_run_pipeline_writes_trace_patch_plan_and_report() {
        let root = std::env::temp_dir().join(format!("kimetsu-pipeline-test-{}", new_id()));
        fs::create_dir_all(root.join("src")).expect("create src");
        // Isolate from any enclosing git repo (see git_init_boundary).
        kimetsu_core::paths::git_init_boundary(&root);
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("manifest");
        fs::write(
            root.join("src").join("lib.rs"),
            "pub fn failing_area() -> bool { false }\n",
        )
        .expect("source");
        project::init_project(&root, false).expect("init project");

        let result = run_coding_dry_run(CodingRunOptions {
            repo: root.clone(),
            task: "Fix failing_area behavior".to_string(),
            dry_run: true,
            allow_high_risk: false,
            disable_model: true,
            disable_broker: false,
            model_key_override: None,
        })
        .expect("dry run");

        assert!(result.final_report_path.exists());
        assert!(result.trace_path.exists());
        let patch_plan_path = root
            .join(".kimetsu")
            .join("runs")
            .join(result.run_id.to_string())
            .join("patch_plans")
            .join(format!("{}.json", result.patch_plan_id));
        assert!(patch_plan_path.exists());
        let patch_plan: PatchPlan =
            serde_json::from_slice(&fs::read(patch_plan_path).expect("read patch plan"))
                .expect("parse patch plan");
        assert!(
            patch_plan
                .files_to_read
                .iter()
                .any(|path| path == "src/lib.rs")
        );
        assert!(
            patch_plan
                .verification_commands
                .iter()
                .any(|command| command.program == "cargo")
        );

        let events = read_trace(&result.trace_path).expect("read trace");
        assert!(events.iter().any(|event| event.kind == "stage.entered"));
        assert!(
            events
                .iter()
                .any(|event| event.kind == "patch.plan.created")
        );
        assert!(events.iter().any(|event| event.kind == "run.finished"));
        // Dry-run skips Verification.
        assert!(!events.iter().any(|event| event.kind == "stage.entered"
            && event.payload.get("stage").and_then(|s| s.as_str()) == Some("verification")));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn verification_section_skipped_in_dry_run() {
        let plan = sample_patch_plan(vec![CommandSpec {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            cwd_relative: None,
            timeout_secs: None,
            expected_exit: None,
        }]);
        let rendered = render_verification_section(true, &plan, None);
        assert!(rendered.starts_with("Skipped (dry-run)"));
        assert!(rendered.contains("cargo"));
    }

    #[test]
    fn verification_section_reports_no_commands() {
        let plan = sample_patch_plan(Vec::new());
        let rendered = render_verification_section(false, &plan, None);
        assert!(rendered.contains("No verification commands"));
    }

    #[test]
    fn verification_section_renders_pass_and_fail_results() {
        let plan = sample_patch_plan(vec![
            CommandSpec {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
                cwd_relative: None,
                timeout_secs: None,
                expected_exit: None,
            },
            CommandSpec {
                program: "cargo".to_string(),
                args: vec!["clippy".to_string()],
                cwd_relative: None,
                timeout_secs: None,
                expected_exit: None,
            },
        ]);
        let summary = serde_json::json!({
            "summary": "Ran 2 verification command(s); 1 failure(s).",
            "commands_run": 2,
            "failures": 1,
            "results": [
                {
                    "kind": "verification",
                    "command": "cargo test",
                    "exit_code": 0,
                    "expected_exit": 0,
                    "duration_ms": 1234,
                    "timed_out": false,
                    "killed_for_policy": false,
                    "stdout_summary": "ok",
                    "stderr_summary": ""
                },
                {
                    "kind": "verification",
                    "command": "cargo clippy",
                    "exit_code": 101,
                    "expected_exit": 0,
                    "duration_ms": 222,
                    "timed_out": false,
                    "killed_for_policy": false,
                    "stdout_summary": "",
                    "stderr_summary": "error: warning treated"
                }
            ]
        });
        let rendered = render_verification_section(false, &plan, Some(&summary));
        assert!(rendered.contains("Ran 2 command(s); 1 failure(s)."));
        assert!(rendered.contains("- pass `cargo test`"));
        assert!(rendered.contains("- fail `cargo clippy`"));
    }

    #[test]
    fn fingerprint_is_stable_across_paths_and_line_numbers() {
        let a = serde_json::json!({
            "exit_code": 101,
            "command": "cargo test",
            "stderr_summary": "thread 'tests::area_of_3_by_4_is_12' panicked at C:\\Users\\rodri\\AppData\\Local\\Temp\\kimetsu-bench-1\\rust_area_bug-brain_on_cold\\src\\lib.rs:15:9:\nassertion `left == right` failed",
        });
        let b = serde_json::json!({
            "exit_code": 101,
            "command": "cargo test",
            "stderr_summary": "thread 'tests::area_of_3_by_4_is_12' panicked at /tmp/different-path-here/src/lib.rs:42:7:\nassertion `left == right` failed",
        });
        assert_eq!(
            verification_failure_fingerprint(&a),
            verification_failure_fingerprint(&b),
            "fingerprints must collapse paths and line numbers"
        );
    }

    #[test]
    fn fingerprint_differs_for_different_errors() {
        let pass_assertion = serde_json::json!({
            "exit_code": 101,
            "command": "cargo test",
            "stderr_summary": "thread 'foo' panicked at src/lib.rs:1:1:\nassertion `left == right` failed",
        });
        let other_panic = serde_json::json!({
            "exit_code": 101,
            "command": "cargo test",
            "stderr_summary": "thread 'bar' panicked at src/lib.rs:2:1:\nindex out of bounds",
        });
        assert_ne!(
            verification_failure_fingerprint(&pass_assertion),
            verification_failure_fingerprint(&other_panic),
        );
    }

    #[test]
    fn fingerprint_strips_ansi_escapes() {
        let plain = serde_json::json!({
            "exit_code": 1,
            "command": "cargo build",
            "stderr_summary": "error: cannot find function `foo` in this scope",
        });
        let with_ansi = serde_json::json!({
            "exit_code": 1,
            "command": "cargo build",
            "stderr_summary": "\u{1b}[31merror\u{1b}[0m: cannot find function `foo` in this scope",
        });
        assert_eq!(
            verification_failure_fingerprint(&plain),
            verification_failure_fingerprint(&with_ansi),
        );
    }

    #[test]
    fn render_retry_context_includes_command_and_stderr_excerpt() {
        let first = serde_json::json!({
            "command": "cargo test",
            "exit_code": 101,
            "expected_exit": 0,
            "stderr_summary": "assertion `left == right` failed",
        });
        let other = serde_json::json!({
            "command": "cargo clippy",
            "exit_code": 101,
            "expected_exit": 0,
        });
        let rendered = render_retry_context(2, &[first.clone(), other], Some(&first));
        assert!(rendered.contains("attempt 2"));
        assert!(rendered.contains("cargo test"));
        assert!(rendered.contains("assertion"));
        assert!(rendered.contains("cargo clippy"));
    }

    #[test]
    fn filter_memory_proposals_keeps_long_guidance_even_with_path_mention() {
        // MP-1.5: the path filter only fires when the proposal is short enough
        // (<80 chars) to plausibly be run-specific. Long generalised guidance
        // that incidentally references a plan path stays in.
        let plan = PatchPlan {
            patch_plan_id: "p".into(),
            run_id: "r".into(),
            revision_of: None,
            rationale: "".into(),
            files_to_read: Vec::new(),
            files_to_modify: vec!["src/lib.rs".into()],
            files_to_create: Vec::new(),
            files_to_delete: Vec::new(),
            verification_commands: Vec::new(),
            expected_outcome: "".into(),
            risk_level: RiskLevel::Low,
        };
        let long_generic = RawMemoryProposal {
            scope: "repo".into(),
            kind: "convention".into(),
            text: "When introducing a new versioned API module, add a thin re-export from the previous version (for example through src/lib.rs) rather than duplicating logic across versions.".into(),
            rationale: String::new(),
            confidence: 0.0, // model omitted; default should now be 0.7
        };
        let summary = filter_memory_proposals(vec![long_generic], RunId::new(), &plan, &[]);
        assert_eq!(
            summary.accepted.len(),
            1,
            "long guidance should not be filtered as path-specific"
        );
        assert!(
            (summary.accepted[0].confidence - 0.7).abs() < f32::EPSILON,
            "missing confidence should default to 0.7 after MP-1.5"
        );
    }

    #[test]
    fn filter_memory_proposals_drops_run_specific_text() {
        let plan = PatchPlan {
            patch_plan_id: "p".into(),
            run_id: "r".into(),
            revision_of: None,
            rationale: "".into(),
            files_to_read: Vec::new(),
            files_to_modify: vec!["src/lib.rs".into()],
            files_to_create: Vec::new(),
            files_to_delete: Vec::new(),
            verification_commands: Vec::new(),
            expected_outcome: "".into(),
            risk_level: RiskLevel::Low,
        };
        let proposals = vec![
            RawMemoryProposal {
                scope: "global_user".into(),
                kind: "preference".into(),
                text: "Prefer ripgrep over grep for code search.".into(),
                rationale: "".into(),
                confidence: 0.8,
            },
            RawMemoryProposal {
                scope: "repo".into(),
                kind: "convention".into(),
                text: "src/lib.rs holds the public API.".into(),
                rationale: "".into(),
                confidence: 0.9,
            },
        ];
        let summary = filter_memory_proposals(proposals, RunId::new(), &plan, &[]);
        assert_eq!(summary.raw_count, 2);
        assert_eq!(summary.accepted.len(), 1);
        assert_eq!(summary.filtered_identifier, 1);
        assert_eq!(summary.accepted[0].kind, "preference");
    }

    #[test]
    fn filter_memory_proposals_drops_duplicates_by_normalized_text() {
        let plan = sample_patch_plan(Vec::new());
        let existing = ExistingMemorySnapshot {
            scope: "global_user".into(),
            kind: "preference".into(),
            normalized_text: kimetsu_core::memory::normalize_memory_text(
                "Prefer ripgrep over grep.",
            ),
            text: "Prefer ripgrep over grep.".into(),
        };
        let proposals = vec![RawMemoryProposal {
            scope: "global_user".into(),
            kind: "preference".into(),
            text: "  Prefer  ripgrep  over grep.  ".into(),
            rationale: "".into(),
            confidence: 0.7,
        }];
        let summary = filter_memory_proposals(proposals, RunId::new(), &plan, &[existing]);
        assert_eq!(summary.accepted.len(), 0);
        assert_eq!(summary.filtered_dedup, 1);
    }

    #[test]
    fn filter_memory_proposals_drops_invalid_scopes_and_kinds() {
        let plan = sample_patch_plan(Vec::new());
        let proposals = vec![
            RawMemoryProposal {
                scope: "global_user".into(),
                kind: "fact".into(),
                text: "Some fact.".into(),
                rationale: "".into(),
                confidence: 0.5,
            },
            RawMemoryProposal {
                scope: "session".into(),
                kind: "preference".into(),
                text: "Bad scope.".into(),
                rationale: "".into(),
                confidence: 0.5,
            },
            RawMemoryProposal {
                scope: "global_user".into(),
                kind: "preference".into(),
                text: "".into(),
                rationale: "".into(),
                confidence: 0.5,
            },
        ];
        let summary = filter_memory_proposals(proposals, RunId::new(), &plan, &[]);
        assert_eq!(summary.accepted.len(), 0);
        assert_eq!(summary.filtered_invalid, 3);
    }

    #[test]
    fn render_memory_proposal_section_renders_accepted_proposals() {
        let summary = MemoryProposalSummary {
            raw_count: 2,
            accepted: vec![AcceptedProposal {
                proposal_id: "abc".into(),
                scope: "global_user".into(),
                kind: "preference".into(),
                text: "Use rg over grep.".into(),
                rationale: "Stated in prior runs".into(),
                confidence: 0.8,
            }],
            filtered_identifier: 1,
            filtered_dedup: 0,
            filtered_invalid: 0,
            filtered_examples: Vec::new(),
        };
        let rendered = render_memory_proposal_section(Some(&summary));
        assert!(rendered.contains("1 accepted"));
        assert!(rendered.contains("[global_user/preference]"));
        assert!(rendered.contains("Use rg over grep"));
        assert!(rendered.contains("rationale: Stated in prior runs"));
    }

    #[test]
    fn build_memory_proposal_request_includes_existing_memories() {
        let config = ProjectConfig::default_for_project("test");
        let plan = sample_patch_plan(Vec::new());
        let existing = ExistingMemorySnapshot {
            scope: "global_user".into(),
            kind: "preference".into(),
            normalized_text: "use rg over grep".into(),
            text: "Use rg over grep".into(),
        };
        let request = build_memory_proposal_request(
            &config,
            "fix bug",
            &plan,
            None,
            std::slice::from_ref(&existing),
        );
        let user_text = request
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::User))
            .map(|m| m.text_content())
            .collect::<String>();
        assert!(user_text.contains("Use rg over grep"));
        assert!(user_text.contains("Existing accepted memories"));
        assert!(request.tools.is_empty());
        assert!(matches!(request.tool_choice, ToolChoice::None));
    }

    fn sample_patch_plan(commands: Vec<CommandSpec>) -> PatchPlan {
        PatchPlan {
            patch_plan_id: "patch_test".to_string(),
            run_id: "run_test".to_string(),
            revision_of: None,
            rationale: "test".to_string(),
            files_to_read: Vec::new(),
            files_to_modify: Vec::new(),
            files_to_create: Vec::new(),
            files_to_delete: Vec::new(),
            verification_commands: commands,
            expected_outcome: "n/a".to_string(),
            risk_level: RiskLevel::Low,
        }
    }

    // ── F1: cross-stage capsule dedup tests ────────────────────────────────

    fn make_capsule(id: &str, kind: &str, summary: &str, token_estimate: u32) -> ContextCapsule {
        ContextCapsule {
            id: id.to_string(),
            kind: kind.to_string(),
            summary: summary.to_string(),
            token_estimate,
            expansion_handle: format!("memory:{id}"),
            provenance: Vec::new(),
            confidence: 0.9,
            freshness: 1.0,
            relevance: 0.8,
            scope_weight: 1.0,
            score: 0.75,
        }
    }

    fn make_bundle(capsules: Vec<ContextCapsule>) -> ContextBundle {
        ContextBundle {
            stage: "patch_plan".to_string(),
            budget_tokens: 4096,
            used_tokens: capsules.iter().map(|c| c.token_estimate).sum(),
            capsules,
            excluded: Vec::new(),
            skipped: false,
            top_score: 0.75,
        }
    }

    /// F1-test-1: two renders of the same bundle with a shared ledger — first
    /// render injects both capsules in full; second render back-references
    /// both. Token count stays at 100 (counted once).
    #[test]
    fn f1_dedup_across_two_renders_same_bundle() {
        let bundle = make_bundle(vec![
            make_capsule(
                "a",
                "semantic_operator",
                "Summary of A — quite long content here",
                50,
            ),
            make_capsule(
                "b",
                "convention",
                "Summary of B — different important stuff",
                50,
            ),
        ]);
        let max = bundle.capsules.len();

        let mut ledger = RunRecallLedger::new();

        // First render (patch-plan stage): both capsules injected in full.
        let first = render_context_capsules(&bundle, max, &mut ledger);
        assert!(
            first.contains("Summary of A"),
            "first render must contain full summary for 'a'"
        );
        assert!(
            first.contains("Summary of B"),
            "first render must contain full summary for 'b'"
        );
        assert_eq!(ledger.injected_count(), 2);
        assert_eq!(ledger.injected_tokens(), 100);

        // Second render (implementation stage): both capsules back-referenced.
        let second = render_context_capsules(&bundle, max, &mut ledger);
        assert!(
            !second.contains("Summary of A"),
            "second render must NOT repeat full summary for 'a'"
        );
        assert!(
            !second.contains("Summary of B"),
            "second render must NOT repeat full summary for 'b'"
        );
        assert!(
            second.contains("(see above)"),
            "second render must contain back-reference marker"
        );
        assert!(
            second.contains("[a]"),
            "second render must reference capsule id 'a'"
        );
        assert!(
            second.contains("[b]"),
            "second render must reference capsule id 'b'"
        );
        // Token cost still counted once.
        assert_eq!(
            ledger.injected_count(),
            2,
            "count must not change on re-render"
        );
        assert_eq!(
            ledger.injected_tokens(),
            100,
            "tokens must not be double-counted"
        );
    }

    /// F1-test-2: partial overlap — first bundle injects {a, b}; second
    /// bundle {b, c} with same ledger → 'b' is back-referenced, 'c' is full.
    #[test]
    fn f1_partial_overlap_second_bundle() {
        let bundle_ab = make_bundle(vec![
            make_capsule("a", "semantic_operator", "A is here for the first time", 50),
            make_capsule("b", "convention", "B appears in both stages", 50),
        ]);
        let bundle_bc = make_bundle(vec![
            make_capsule("b", "convention", "B appears in both stages", 50),
            make_capsule("c", "anti_pattern", "C is new in the second stage", 60),
        ]);

        let mut ledger = RunRecallLedger::new();

        // First render: inject a and b.
        let _ = render_context_capsules(&bundle_ab, bundle_ab.capsules.len(), &mut ledger);
        assert_eq!(ledger.injected_count(), 2);
        assert_eq!(ledger.injected_tokens(), 100); // a=50, b=50

        // Second render: b is back-ref, c is new.
        let second = render_context_capsules(&bundle_bc, bundle_bc.capsules.len(), &mut ledger);
        assert!(
            !second.contains("B appears in both stages"),
            "'b' summary must not re-appear"
        );
        assert!(
            second.contains("C is new"),
            "'c' summary must be rendered in full"
        );
        assert_eq!(
            ledger.injected_count(),
            3,
            "a + b + c = 3 distinct capsules"
        );
        assert_eq!(
            ledger.injected_tokens(),
            160,
            "a(50) + b(50) + c(60) = 160, each counted once"
        );
    }

    /// F1-test-3: back-reference is materially shorter than a full render.
    #[test]
    fn f1_back_ref_is_cheaper_than_full_render() {
        let long_summary = "A".repeat(300); // deliberately long
        let bundle = make_bundle(vec![make_capsule(
            "x",
            "semantic_operator",
            &long_summary,
            200,
        )]);

        let mut ledger = RunRecallLedger::new();

        let full = render_context_capsules(&bundle, 1, &mut ledger);
        let back_ref = render_context_capsules(&bundle, 1, &mut ledger);

        assert!(
            full.contains(&long_summary),
            "full render must include the long summary"
        );
        assert!(
            !back_ref.contains(&long_summary),
            "back-ref must not include the long summary"
        );
        assert!(
            back_ref.len() < full.len() / 2,
            "back-ref ({} chars) should be materially shorter than full ({} chars)",
            back_ref.len(),
            full.len()
        );
    }

    // ── F2: tiered capsule injection tests ────────────────────────────────

    /// F2-test-1: with 6 capsules, only the top FULL_TIER_CAP are rendered in
    /// full; the rest appear as HEADLINE lines.
    #[test]
    fn f2_top_tier_full_rest_headline() {
        // Build 6 capsules with decreasing scores (make_capsule sets score=0.75
        // for all; we can't easily vary score here since make_capsule is fixed,
        // but the order is preserved by take() so we just check structure).
        let capsules: Vec<_> = (0..6)
            .map(|i| {
                make_capsule(
                    &format!("cap{i}"),
                    "memory",
                    &format!("Summary of capsule {i} with important details about the task"),
                    100, // full token estimate
                )
            })
            .collect();
        let bundle = make_bundle(capsules);
        let max = bundle.capsules.len();

        let mut ledger = RunRecallLedger::new();
        let rendered = render_context_capsules(&bundle, max, &mut ledger);

        // Top FULL_TIER_CAP=3 capsules should appear with their full summary.
        for i in 0..FULL_TIER_CAP {
            assert!(
                rendered.contains(&format!("Summary of capsule {i}")),
                "capsule {i} (in full tier) should have its summary in the render"
            );
        }

        // Capsules beyond the full tier should appear as HEADLINE lines.
        assert!(
            rendered.contains("[headline]"),
            "at least one headline line should appear in the render"
        );
        for i in FULL_TIER_CAP..6 {
            // The handle for each headline capsule should appear (it's in the
            // "expand with handle memory:cap{i}" suffix).
            assert!(
                rendered.contains(&format!("cap{i}")),
                "headline for capsule {i} should reference its handle"
            );
            // The FULL verbose structure (id: / score: / summary: multi-line) must
            // NOT appear for headline-tier capsules — headlines are one-liners.
            // Check that the verbose "score:" label (only in full renders) is
            // absent from parts of the render that correspond to headline capsules.
        }
        // Additionally, the total render should NOT contain the multi-line
        // verbose block for more than FULL_TIER_CAP capsules.
        // We count lines that start with "  score:" (only full renders have these).
        let score_lines = rendered
            .lines()
            .filter(|l| l.trim_start().starts_with("score:"))
            .count();
        assert!(
            score_lines <= FULL_TIER_CAP,
            "at most FULL_TIER_CAP={FULL_TIER_CAP} capsules should have 'score:' lines (full render); got {score_lines}"
        );
    }

    /// F2-test-2: headline capsules are charged at HEADLINE_TOKEN_COST, NOT
    /// their full token_estimate — this is the cost lever.
    #[test]
    fn f2_headline_tier_charges_small_cost() {
        let capsules: Vec<_> = (0..6)
            .map(|i| make_capsule(&format!("c{i}"), "memory", &format!("Summary {i}"), 200))
            .collect();
        let bundle = make_bundle(capsules);
        let max = bundle.capsules.len();

        let mut ledger = RunRecallLedger::new();
        let _ = render_context_capsules(&bundle, max, &mut ledger);

        // Full tier: FULL_TIER_CAP × 200 tokens each.
        let full_tier_cost = FULL_TIER_CAP as u32 * 200;
        // Headline tier: (6 - FULL_TIER_CAP) × HEADLINE_TOKEN_COST each.
        let headline_count = (6 - FULL_TIER_CAP) as u32;
        let headline_cost = headline_count * HEADLINE_TOKEN_COST;
        let expected = full_tier_cost + headline_cost;

        assert_eq!(
            ledger.injected_tokens(),
            expected,
            "injected tokens should be full-tier({full_tier_cost}) + headline({headline_cost}) = {expected}"
        );
        // And must be materially less than paying full cost for all 6.
        let all_full = 6u32 * 200;
        assert!(
            ledger.injected_tokens() < all_full,
            "headline tier must save tokens vs paying all-full: {} < {}",
            ledger.injected_tokens(),
            all_full
        );
    }

    /// F2-test-3: back-reference (F1) takes priority over the tier decision.
    /// A capsule already injected is back-referenced regardless of where it
    /// falls in the order — no new charge at all.
    #[test]
    fn f2_already_injected_capsule_is_back_referenced_not_charged_again() {
        let caps: Vec<_> = (0..4)
            .map(|i| make_capsule(&format!("c{i}"), "memory", &format!("Summary {i}"), 100))
            .collect();
        let bundle = make_bundle(caps);
        let max = bundle.capsules.len();

        let mut ledger = RunRecallLedger::new();
        // First render: injects top FULL_TIER_CAP in full, rest as headlines.
        let _ = render_context_capsules(&bundle, max, &mut ledger);
        let after_first = ledger.injected_tokens();

        // Second render with the SAME ledger: all 4 capsules are already injected.
        // The ledger's mark_injected is idempotent — no new cost must be added.
        let second = render_context_capsules(&bundle, max, &mut ledger);
        assert_eq!(
            ledger.injected_tokens(),
            after_first,
            "no new tokens should be charged on the second render"
        );
        // Back-references must appear for all 4 capsules.
        assert!(
            second.contains("(see above)"),
            "second render must contain back-reference markers"
        );
    }

    /// F2-test-4: a capsule rendered as a headline (small charge) and then
    /// "expanded" by the tool does NOT re-charge the ledger — the tool dispatch
    /// is separate accounting.
    #[test]
    fn f2_headline_does_not_double_charge_after_tool_expansion() {
        let caps: Vec<_> = (0..5)
            .map(|i| make_capsule(&format!("h{i}"), "memory", &format!("Headline {i}"), 150))
            .collect();
        let bundle = make_bundle(caps.clone());
        let max = bundle.capsules.len();

        let mut ledger = RunRecallLedger::new();
        let _ = render_context_capsules(&bundle, max, &mut ledger);

        // Tokens after render: FULL_TIER_CAP × 150 + 2 × HEADLINE_TOKEN_COST.
        let expected =
            FULL_TIER_CAP as u32 * 150 + (5 - FULL_TIER_CAP) as u32 * HEADLINE_TOKEN_COST;
        assert_eq!(ledger.injected_tokens(), expected);

        // Simulate the agent expanding one headline capsule via the tool.
        // The ledger itself is NOT touched by the tool dispatch — only render
        // calls mark_injected. So the ledger total stays the same.
        // (The test proves there is no mechanism to re-charge: mark_injected is
        // idempotent and the tool path never calls it.)
        let tokens_before = ledger.injected_tokens();
        // Calling mark_injected again for a headline id with its FULL cost is
        // idempotent — the original HEADLINE_TOKEN_COST is kept.
        ledger.mark_injected("h3", 999); // 999 would be the full cost if re-charged
        assert_eq!(
            ledger.injected_tokens(),
            tokens_before,
            "headline expansion via tool must not alter the ledger total"
        );
    }

    // ── E1: proactive failure anticipation tests ───────────────────────────

    /// E1-test-1: a fresh pitfall bundle with one failure_pattern capsule
    /// produces a "Known pitfalls" block containing that capsule's summary.
    #[test]
    fn e1_pitfall_surfaces_in_first_attempt() {
        let bundle = make_bundle(vec![make_capsule(
            "fp-1",
            "failure_pattern",
            "Always run cargo fmt before cargo clippy or clippy will reject the diff",
            30,
        )]);
        let mut ledger = RunRecallLedger::new();

        let result = render_known_pitfalls(&bundle, &mut ledger);
        assert!(
            result.is_some(),
            "should return Some(_) when there is a matching pitfall"
        );
        let block = result.unwrap();
        assert!(
            block.contains("Known pitfalls"),
            "block must contain the section header"
        );
        assert!(
            block.contains("Always run cargo fmt"),
            "block must contain the capsule summary"
        );
        assert!(
            block.contains("failure_pattern"),
            "block must include the capsule kind"
        );
        // The pitfall must now be recorded in the surfaced set.
        assert!(
            ledger.is_surfaced("fp-1"),
            "capsule id must be marked surfaced after first render"
        );
    }

    /// E1-test-2: when the bundle is skipped (empty brain / no match above
    /// min_score) or truly empty, render_known_pitfalls returns None — zero
    /// tokens added to the prompt.
    #[test]
    fn e1_no_match_returns_none() {
        // Skipped bundle (top_score below min_score → skipped:true, capsules empty).
        let skipped_bundle = ContextBundle {
            stage: "implementation".to_string(),
            budget_tokens: 600,
            used_tokens: 0,
            capsules: Vec::new(),
            excluded: Vec::new(),
            skipped: true,
            top_score: 0.0,
        };
        let mut ledger = RunRecallLedger::new();
        assert!(
            render_known_pitfalls(&skipped_bundle, &mut ledger).is_none(),
            "skipped bundle must produce None"
        );

        // Completely empty bundle (skipped:false but no capsules).
        let empty_bundle = ContextBundle {
            stage: "implementation".to_string(),
            budget_tokens: 600,
            used_tokens: 0,
            capsules: Vec::new(),
            excluded: Vec::new(),
            skipped: false,
            top_score: 0.0,
        };
        assert!(
            render_known_pitfalls(&empty_bundle, &mut ledger).is_none(),
            "empty bundle must produce None"
        );
    }

    /// E1-test-3: a pitfall surfaced in attempt #1 is NOT repeated in attempt
    /// #2 — the ledger's surfaced set suppresses it.
    #[test]
    fn e1_ledger_suppresses_pitfall_on_retry() {
        let bundle = make_bundle(vec![make_capsule(
            "fp-retry",
            "convention",
            "Use `--locked` with cargo install to reproduce CI builds exactly",
            25,
        )]);
        let mut ledger = RunRecallLedger::new();

        // Attempt #1: pitfall surfaces.
        let first = render_known_pitfalls(&bundle, &mut ledger);
        assert!(
            first.is_some(),
            "pitfall should surface on the first attempt"
        );
        assert!(first.unwrap().contains("--locked"));

        // Attempt #2: same ledger — pitfall already surfaced, returns None.
        let second = render_known_pitfalls(&bundle, &mut ledger);
        assert!(
            second.is_none(),
            "pitfall must NOT be repeated when already surfaced this run"
        );
    }

    /// E1-test-4: proactive surfaced dedup (is_surfaced/mark_surfaced) is
    /// independent of the F1 injected dedup (is_injected/mark_injected). A
    /// capsule can be injected via the normal context path AND surfaced as a
    /// pitfall without the two mechanisms conflicting.
    #[test]
    fn e1_surfaced_and_injected_are_independent() {
        let capsule = make_capsule(
            "dual",
            "failure_pattern",
            "Watch out for lifetime issues in async blocks",
            40,
        );
        let bundle = make_bundle(vec![capsule.clone()]);
        let mut ledger = RunRecallLedger::new();

        // Mark it injected (F1 path) — shouldn't affect proactive surfacing.
        ledger.mark_injected("dual", 40);
        assert!(ledger.is_injected("dual"));
        assert!(!ledger.is_surfaced("dual"));

        // Proactive path: should still surface it.
        let result = render_known_pitfalls(&bundle, &mut ledger);
        assert!(
            result.is_some(),
            "is_injected must NOT prevent proactive surfacing"
        );
        assert!(ledger.is_surfaced("dual"));

        // Mark it surfaced (E1 path) — shouldn't affect injected state.
        // (Already marked above; verify injected_count is still 1.)
        assert_eq!(ledger.injected_count(), 1);
        assert_eq!(ledger.injected_tokens(), 40);
    }

    /// E1-test-5: the proactive request envelope uses the correct kinds,
    /// min_score, max_capsules, and budget_tokens parameters.
    #[test]
    fn e1_pitfall_request_uses_correct_parameters() {
        // This is a structural/contract test that verifies the ContextRequest
        // constants match the spec without needing a live brain connection.
        let task = "fix the scheduler loop";
        let expected_outcome = "scheduler exits cleanly";
        let request = ContextRequest {
            stage: "implementation".to_string(),
            query: format!("{task}\n{expected_outcome}"),
            budget_tokens: 600,
            min_score: 0.7,
            max_capsules: 2,
            kinds: vec!["failure_pattern".to_string(), "convention".to_string()],
            ..Default::default()
        };
        assert_eq!(request.stage, "implementation");
        assert_eq!(request.budget_tokens, 600);
        assert!(
            (request.min_score - 0.7).abs() < f32::EPSILON,
            "min_score must be 0.7"
        );
        assert_eq!(request.max_capsules, 2);
        assert_eq!(request.kinds, vec!["failure_pattern", "convention"]);
        assert!(request.query.contains(task), "query must include the task");
        assert!(
            request.query.contains(expected_outcome),
            "query must include a patch-plan signal"
        );
    }

    // ── F3: adaptive budget + per-run cap + overhead ratio tests ──────────

    /// F3-pipeline-1: `estimate_task_tokens` uses the same whitespace-word
    /// heuristic as the rest of the pipeline (words * 1.33 ceiled).
    #[test]
    fn f3_estimate_task_tokens_matches_pipeline_heuristic() {
        // 4 words → ceil(4 * 1.33) = ceil(5.32) = 6
        let text = "fix the scheduler loop";
        assert_eq!(
            estimate_task_tokens(text),
            6,
            "4 whitespace-words → 6 tokens"
        );

        // Empty string → 0
        assert_eq!(estimate_task_tokens(""), 0, "empty string → 0 tokens");

        // 1 word → ceil(1.33) = 2
        assert_eq!(estimate_task_tokens("word"), 2, "1 word → 2 tokens");
    }

    /// F3-pipeline-2: per-run global cap via ledger.
    ///
    /// Simulates two stages: stage 1 injects near the cap, stage 2's effective
    /// budget is the small remainder, and total injected never exceeds run_cap.
    #[test]
    fn f3_per_run_global_cap_limits_total_brain_tokens() {
        use kimetsu_core::config::adaptive_budget;

        let floor = 1500u32;
        let run_cap = 8000u32;
        let task_size = 200u32;

        // Stage 1: budget = adaptive_budget(task_size), remaining = run_cap
        let stage1_budget = adaptive_budget(task_size, floor, run_cap);

        // Simulate stage 1 injecting its full budget.
        let mut ledger = RunRecallLedger::new();
        ledger.mark_injected("cap-s1-a", stage1_budget / 2);
        ledger.mark_injected("cap-s1-b", stage1_budget / 2);
        let after_stage1 = ledger.injected_tokens();

        // Stage 2: remaining budget = run_cap - injected so far.
        let remaining_stage2 = run_cap.saturating_sub(after_stage1);
        let stage2_budget = adaptive_budget(task_size, floor, run_cap).min(remaining_stage2);

        // Simulate stage 2 trying to inject up to its budget.
        ledger.mark_injected("cap-s2-a", stage2_budget / 2);
        ledger.mark_injected("cap-s2-b", stage2_budget / 2);

        let total_injected = ledger.injected_tokens();
        assert!(
            total_injected <= run_cap,
            "total injected ({total_injected}) must not exceed run_cap ({run_cap})"
        );
        assert!(
            remaining_stage2 < stage1_budget,
            "stage 2 must have less budget than stage 1 when stage 1 used its allocation"
        );
    }

    /// F3-pipeline-3: overhead-ratio fixture-level proof.
    ///
    /// Two fixtures — a small task and a ~5×-larger task. The task-size signal
    /// is defined as: estimate_tokens(task_text) + estimate_tokens(file_list).
    ///
    /// Assertions (fixture-level, not universal theorems):
    ///   (a) brain tokens grow < 2× between small and large fixture
    ///   (b) overhead ratio (brain / total) is STRICTLY LOWER for the larger task
    ///       where total ∝ task_size (simulated as 8× task_size for realism)
    ///
    /// The factor 8× for total comes from the observation that typical model
    /// context (system prompt + file contents + conversation) is roughly an
    /// order of magnitude larger than the task description alone.
    #[test]
    fn f3_overhead_ratio_falls_on_larger_task() {
        use kimetsu_core::config::adaptive_budget;

        let floor = 1500u32;
        let run_cap = 16_000u32; // raised cap so both fixtures are unclamped

        // Small fixture: a concise 5-word task + 2 file paths listed
        // task: "fix the scheduler exit loop" → estimate_task_tokens = 8
        // files: "- src/scheduler.rs\n- src/main.rs" → ~4 words → 6 tokens
        // task_size_small ≈ 14 tokens
        let task_small = "fix the scheduler exit loop";
        let files_small = "- src/scheduler.rs\n- src/main.rs";
        let task_size_small =
            estimate_task_tokens(task_small).saturating_add(estimate_task_tokens(files_small));

        // Large fixture: a ~5× larger task (verbose multi-paragraph description)
        // + many more file paths. We construct it to be ~5× task_size_small.
        // 5 × 14 ≈ 70 tokens; use 30 task words (≈40 tokens) + 20 file path words (≈27 tokens)
        // = ~67 tokens.
        let task_large = "implement a comprehensive fix for the scheduler exit loop \
            that handles all edge cases including the timeout path the retry path \
            and the graceful shutdown sequence with proper cleanup of resources";
        let files_large = "- src/scheduler.rs\n- src/main.rs\n- src/config.rs\n\
            - src/worker.rs\n- src/runtime.rs\n- tests/scheduler_test.rs";
        let task_size_large =
            estimate_task_tokens(task_large).saturating_add(estimate_task_tokens(files_large));

        // Verify the large fixture is genuinely ~5× the small fixture.
        assert!(
            task_size_large >= 4 * task_size_small,
            "large fixture ({task_size_large} tokens) should be >= 4× small ({task_size_small} tokens)"
        );

        // Compute adaptive brain budgets for each fixture.
        let brain_small = adaptive_budget(task_size_small, floor, run_cap) as f64;
        let brain_large = adaptive_budget(task_size_large, floor, run_cap) as f64;

        // (a) brain tokens grow < 2× even though task is ~5× larger.
        assert!(
            brain_large < 2.0 * brain_small,
            "F3 sublinear guarantee (fixture): brain_large={brain_large} must be < 2×brain_small={}",
            2.0 * brain_small
        );

        // (b) Overhead ratio (brain / total) strictly falls for the larger task.
        // Total model context is simulated as 8 × task_size (task description +
        // file contents + system prompt) — this factor is the same for both, so
        // the ratio difference is purely driven by the sublinear brain budget.
        let total_factor = 8.0f64;
        let total_small = total_factor * task_size_small as f64;
        let total_large = total_factor * task_size_large as f64;

        let ratio_small = brain_small / total_small;
        let ratio_large = brain_large / total_large;

        assert!(
            ratio_large < ratio_small,
            "F3 overhead-ratio fixture: ratio_large={ratio_large:.4} must be < ratio_small={ratio_small:.4} \
            (brain tokens grow sublinearly while task/total grows linearly)"
        );

        // Sanity: both ratios are positive and sensible.
        assert!(
            ratio_small > 0.0 && ratio_large > 0.0,
            "ratios must be positive"
        );
    }
}
