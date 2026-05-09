use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_brain::context::ContextCapsule;
use kimetsu_brain::project;
use kimetsu_brain::trace::read_trace;
use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::new_id;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use serde::{Deserialize, Serialize};

use crate::pipeline::{CodingRunOptions, PatchPlan, run_coding_dry_run};

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub repo: PathBuf,
    pub keep_fixtures: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRunResult {
    pub bench_run_id: String,
    pub task_count: usize,
    pub report_path: PathBuf,
    pub results_path: PathBuf,
    pub summaries: Vec<BenchModeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchModeSummary {
    pub mode: String,
    pub tasks: usize,
    pub success_rate: f32,
    pub relevant_signal_rate: f32,
    pub accepted_memories_used: u32,
    pub context_loads: u32,
    pub irrelevant_context_loaded: u32,
    pub dry_runs: u32,
    pub trace_events: u32,
    pub model_turns: u32,
    pub model_skips: u32,
    pub tool_calls: u32,
    pub verification_attempts: u32,
    pub planned_relevant_files: u32,
    pub unrelated_planned_files: u32,
    pub total_cost_usd: f32,
    pub total_duration_ms: f32,
    pub avg_duration_ms: f32,
    pub stage_profiles: Vec<StageTimeSummary>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageTimeSummary {
    pub stage: String,
    pub runs: u32,
    pub total_duration_ms: f32,
    pub avg_duration_ms: f32,
    pub max_duration_ms: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchReport {
    bench_run_id: String,
    task_count: usize,
    summaries: Vec<BenchModeSummary>,
    results: Vec<BenchTaskResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchTaskResult {
    task_id: String,
    category: String,
    mode: String,
    success: bool,
    relevant_signal_loaded: bool,
    relevant_files_loaded: u32,
    accepted_memories_used: u32,
    context_loads: u32,
    irrelevant_context_loaded: u32,
    included_handles: Vec<String>,
    dry_run: Option<DryRunBenchMetrics>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DryRunBenchMetrics {
    run_id: String,
    trace_artifact: String,
    patch_plan_artifact: String,
    terminal_kind: String,
    duration_us: u64,
    duration_ms: u64,
    trace_events: u32,
    stage_events: u32,
    stage_profiles: Vec<StageTimeProfile>,
    model_turns: u32,
    model_skips: u32,
    tool_calls: u32,
    verification_attempts: u32,
    total_cost_usd: f32,
    planned_files_to_read: u32,
    planned_files_to_modify: u32,
    planned_relevant_files: u32,
    unrelated_planned_files: u32,
    risk_level: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StageTimeProfile {
    stage: String,
    entered_event_id: String,
    completed_event_id: String,
    duration_us: u64,
    duration_ms: f32,
}

#[derive(Debug, Clone)]
struct BenchTask {
    id: &'static str,
    category: &'static str,
    seed_task: &'static str,
    followup_task: &'static str,
    memory_kind: MemoryKind,
    warm_memory: &'static str,
    relevant_handles: Vec<&'static str>,
    files: Vec<FixtureFile>,
}

#[derive(Debug, Clone)]
struct FixtureFile {
    path: &'static str,
    content: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchMode {
    BrainOff,
    BrainOnCold,
    BrainOnWarm,
}

impl BenchMode {
    fn all() -> [Self; 3] {
        [Self::BrainOff, Self::BrainOnCold, Self::BrainOnWarm]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::BrainOff => "brain_off",
            Self::BrainOnCold => "brain_on_cold",
            Self::BrainOnWarm => "brain_on_warm",
        }
    }
}

pub fn run_benchmark(options: BenchOptions) -> KimetsuResult<BenchRunResult> {
    let bench_run_id = new_id().to_string();
    let tasks = bench_tasks();
    let output_dir = options
        .repo
        .canonicalize()
        .unwrap_or(options.repo.clone())
        .join(".kimetsu")
        .join("bench")
        .join(&bench_run_id);
    fs::create_dir_all(&output_dir)?;

    let fixture_root = std::env::temp_dir().join(format!("kimetsu-bench-{bench_run_id}"));
    if fixture_root.exists() {
        fs::remove_dir_all(&fixture_root)?;
    }
    fs::create_dir_all(&fixture_root)?;

    let mut results = Vec::new();
    let run_result = (|| -> KimetsuResult<()> {
        for task in &tasks {
            for mode in BenchMode::all() {
                results.push(run_task_mode(&fixture_root, &output_dir, task, mode)?);
            }
        }
        Ok(())
    })();

    if !options.keep_fixtures && fixture_root.exists() {
        let cleanup = fs::remove_dir_all(&fixture_root);
        if run_result.is_ok() {
            cleanup?;
        }
    }
    run_result?;

    let summaries = summarize_results(&results);
    let report = BenchReport {
        bench_run_id: bench_run_id.clone(),
        task_count: tasks.len(),
        summaries: summaries.clone(),
        results,
    };

    let results_path = output_dir.join("results.json");
    let report_path = output_dir.join("report.md");
    fs::write(&results_path, serde_json::to_vec_pretty(&report)?)?;
    fs::write(&report_path, render_report(&report))?;

    Ok(BenchRunResult {
        bench_run_id,
        task_count: tasks.len(),
        report_path,
        results_path,
        summaries,
    })
}

fn run_task_mode(
    fixture_root: &Path,
    output_dir: &Path,
    task: &BenchTask,
    mode: BenchMode,
) -> KimetsuResult<BenchTaskResult> {
    if mode == BenchMode::BrainOff {
        return Ok(evaluate_capsules(task, mode, Vec::new(), None));
    }

    let repo = fixture_root.join(format!("{}-{}", task.id, mode.as_str()));
    write_fixture_repo(&repo, task)?;
    project::init_project(&repo, false)?;
    if mode == BenchMode::BrainOnWarm {
        project::add_memory(
            &repo,
            MemoryScope::Repo,
            task.memory_kind,
            &format!("[bench:{}] {}", task.id, task.warm_memory),
        )?;
    }
    project::ingest_repo(&repo)?;
    let context = project::retrieve_context(&repo, "patch_plan", task.followup_task, 420)?;
    let dry_run = run_coding_dry_run(CodingRunOptions {
        repo: repo.clone(),
        task: task.followup_task.to_string(),
        dry_run: true,
        allow_high_risk: true,
        disable_model: true,
    })?;
    let dry_run_metrics = dry_run_metrics(output_dir, task, mode, &dry_run.trace_path)?;
    Ok(evaluate_capsules(
        task,
        mode,
        context.capsules,
        Some(dry_run_metrics),
    ))
}

fn evaluate_capsules(
    task: &BenchTask,
    mode: BenchMode,
    capsules: Vec<ContextCapsule>,
    dry_run: Option<DryRunBenchMetrics>,
) -> BenchTaskResult {
    let included_handles = capsules
        .iter()
        .map(|capsule| capsule.expansion_handle.clone())
        .collect::<Vec<_>>();
    let relevant_files_loaded = task
        .relevant_handles
        .iter()
        .filter(|handle| included_handles.iter().any(|loaded| loaded == *handle))
        .count() as u32;
    let accepted_memories_used = capsules
        .iter()
        .filter(|capsule| capsule.expansion_handle.starts_with("memory:"))
        .count() as u32;
    let relevant_memory_loaded = capsules.iter().any(|capsule| {
        capsule.expansion_handle.starts_with("memory:")
            && capsule.summary.contains(&format!("[bench:{}]", task.id))
    });
    let relevant_signal_loaded = relevant_files_loaded > 0 || relevant_memory_loaded;
    let irrelevant_context_loaded = capsules
        .iter()
        .filter(|capsule| {
            let handle = capsule.expansion_handle.as_str();
            let relevant_file = task
                .relevant_handles
                .iter()
                .any(|expected| expected == &handle);
            let relevant_memory = handle.starts_with("memory:")
                && capsule.summary.contains(&format!("[bench:{}]", task.id));
            !relevant_file && !relevant_memory
        })
        .count() as u32;

    BenchTaskResult {
        task_id: task.id.to_string(),
        category: task.category.to_string(),
        mode: mode.as_str().to_string(),
        success: relevant_signal_loaded,
        relevant_signal_loaded,
        relevant_files_loaded,
        accepted_memories_used,
        context_loads: included_handles.len() as u32,
        irrelevant_context_loaded,
        included_handles,
        dry_run,
    }
}

fn dry_run_metrics(
    output_dir: &Path,
    task: &BenchTask,
    mode: BenchMode,
    trace_path: &Path,
) -> KimetsuResult<DryRunBenchMetrics> {
    let events = read_trace(trace_path)?;
    let run_id = events
        .first()
        .map(|event| event.run_id.to_string())
        .unwrap_or_default();
    let terminal_kind = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.kind.as_str(),
                "run.finished" | "run.failed" | "run.aborted"
            )
        })
        .map(|event| event.kind.clone())
        .unwrap_or_else(|| "running".to_string());
    let duration_us = match (events.first(), events.last()) {
        (Some(first), Some(last)) => duration_us(first.ts, last.ts),
        _ => 0,
    };
    let duration_ms = duration_us / 1_000;
    let stage_profiles = stage_time_profiles(&events);
    let model_turns = count_kind(&events, "model.requested");
    let model_skips = count_kind(&events, "model.skipped");
    let tool_calls = count_kind(&events, "tool.called");
    let verification_attempts = events
        .iter()
        .filter(|event| {
            event.kind == "stage.entered"
                && event.payload.get("stage").and_then(|stage| stage.as_str())
                    == Some("verification")
        })
        .count() as u32;
    let stage_events = count_kind(&events, "stage.entered");
    let total_cost_usd = events
        .iter()
        .filter(|event| event.kind == "model.responded")
        .filter_map(|event| event.payload.pointer("/usage/cost_usd"))
        .filter_map(|value| value.as_f64())
        .sum::<f64>() as f32;

    let artifact_dir = output_dir
        .join("artifacts")
        .join(task.id)
        .join(mode.as_str());
    fs::create_dir_all(&artifact_dir)?;
    let trace_artifact = format!("artifacts/{}/{}/trace.jsonl", task.id, mode.as_str());
    fs::copy(trace_path, artifact_dir.join("trace.jsonl"))?;

    let patch_plan_artifact = events
        .iter()
        .rev()
        .find(|event| event.kind == "patch.plan.created")
        .and_then(|event| event.payload.get("artifact"))
        .and_then(|value| value.as_str())
        .ok_or("dry-run trace missing patch.plan.created artifact")?;
    let run_dir = trace_path
        .parent()
        .ok_or("dry-run trace path has no run directory")?;
    let patch_plan_source = run_dir.join(patch_plan_artifact);
    let patch_plan: PatchPlan = serde_json::from_slice(&fs::read(&patch_plan_source)?)?;
    let patch_plan_copy = artifact_dir.join("patch_plan.json");
    fs::copy(&patch_plan_source, &patch_plan_copy)?;
    let patch_plan_artifact = format!("artifacts/{}/{}/patch_plan.json", task.id, mode.as_str());

    let relevant_paths = task
        .relevant_handles
        .iter()
        .filter_map(|handle| handle.strip_prefix("file:"))
        .collect::<Vec<_>>();
    let planned_relevant_files = relevant_paths
        .iter()
        .filter(|path| {
            patch_plan
                .files_to_read
                .iter()
                .any(|planned| planned == **path)
                || patch_plan
                    .files_to_modify
                    .iter()
                    .any(|planned| planned == **path)
                || patch_plan
                    .files_to_create
                    .iter()
                    .any(|planned| planned == **path)
                || patch_plan
                    .files_to_delete
                    .iter()
                    .any(|planned| planned == **path)
        })
        .count() as u32;
    let unrelated_planned_files = patch_plan
        .files_to_modify
        .iter()
        .chain(patch_plan.files_to_create.iter())
        .chain(patch_plan.files_to_delete.iter())
        .filter(|planned| !relevant_paths.iter().any(|path| path == &planned.as_str()))
        .count() as u32;

    Ok(DryRunBenchMetrics {
        run_id,
        trace_artifact,
        patch_plan_artifact,
        terminal_kind,
        duration_us,
        duration_ms,
        trace_events: events.len() as u32,
        stage_events,
        stage_profiles,
        model_turns,
        model_skips,
        tool_calls,
        verification_attempts,
        total_cost_usd,
        planned_files_to_read: patch_plan.files_to_read.len() as u32,
        planned_files_to_modify: patch_plan.files_to_modify.len() as u32,
        planned_relevant_files,
        unrelated_planned_files,
        risk_level: format!("{:?}", patch_plan.risk_level).to_ascii_lowercase(),
    })
}

fn stage_time_profiles(events: &[kimetsu_core::event::Event]) -> Vec<StageTimeProfile> {
    let mut open = BTreeMap::<String, (String, time::OffsetDateTime)>::new();
    let mut profiles = Vec::new();

    for event in events {
        let Some(stage) = event.payload.get("stage").and_then(|stage| stage.as_str()) else {
            continue;
        };

        if event.kind == "stage.entered" {
            open.insert(stage.to_string(), (event.event_id.to_string(), event.ts));
        } else if event.kind == "stage.completed"
            && let Some((entered_event_id, entered_at)) = open.remove(stage)
        {
            let duration_us = duration_us(entered_at, event.ts);
            profiles.push(StageTimeProfile {
                stage: stage.to_string(),
                entered_event_id,
                completed_event_id: event.event_id.to_string(),
                duration_us,
                duration_ms: duration_us as f32 / 1_000.0,
            });
        }
    }

    profiles
}

fn duration_us(start: time::OffsetDateTime, end: time::OffsetDateTime) -> u64 {
    (end - start).whole_microseconds().max(0) as u64
}

fn count_kind(events: &[kimetsu_core::event::Event], kind: &str) -> u32 {
    events.iter().filter(|event| event.kind == kind).count() as u32
}

fn summarize_results(results: &[BenchTaskResult]) -> Vec<BenchModeSummary> {
    let mut grouped = BTreeMap::<String, Vec<&BenchTaskResult>>::new();
    for result in results {
        grouped.entry(result.mode.clone()).or_default().push(result);
    }

    grouped
        .into_iter()
        .map(|(mode, results)| {
            let tasks = results.len();
            let success_count = results.iter().filter(|result| result.success).count();
            let relevant_count = results
                .iter()
                .filter(|result| result.relevant_signal_loaded)
                .count();
            let dry_metrics = results
                .iter()
                .filter_map(|result| result.dry_run.as_ref())
                .collect::<Vec<_>>();
            let total_duration_us = dry_metrics
                .iter()
                .map(|metrics| metrics.duration_us)
                .sum::<u64>();
            let dry_run_count = dry_metrics.len() as u32;
            BenchModeSummary {
                mode,
                tasks,
                success_rate: ratio(success_count, tasks),
                relevant_signal_rate: ratio(relevant_count, tasks),
                accepted_memories_used: results
                    .iter()
                    .map(|result| result.accepted_memories_used)
                    .sum(),
                context_loads: results.iter().map(|result| result.context_loads).sum(),
                irrelevant_context_loaded: results
                    .iter()
                    .map(|result| result.irrelevant_context_loaded)
                    .sum(),
                dry_runs: dry_run_count,
                trace_events: dry_metrics.iter().map(|metrics| metrics.trace_events).sum(),
                model_turns: dry_metrics.iter().map(|metrics| metrics.model_turns).sum(),
                model_skips: dry_metrics.iter().map(|metrics| metrics.model_skips).sum(),
                tool_calls: dry_metrics.iter().map(|metrics| metrics.tool_calls).sum(),
                verification_attempts: dry_metrics
                    .iter()
                    .map(|metrics| metrics.verification_attempts)
                    .sum(),
                planned_relevant_files: dry_metrics
                    .iter()
                    .map(|metrics| metrics.planned_relevant_files)
                    .sum(),
                unrelated_planned_files: dry_metrics
                    .iter()
                    .map(|metrics| metrics.unrelated_planned_files)
                    .sum(),
                total_cost_usd: dry_metrics
                    .iter()
                    .map(|metrics| metrics.total_cost_usd)
                    .sum(),
                total_duration_ms: total_duration_us as f32 / 1_000.0,
                avg_duration_ms: if dry_run_count == 0 {
                    0.0
                } else {
                    total_duration_us as f32 / 1_000.0 / dry_run_count as f32
                },
                stage_profiles: summarize_stage_profiles(&dry_metrics),
            }
        })
        .collect()
}

fn summarize_stage_profiles(metrics: &[&DryRunBenchMetrics]) -> Vec<StageTimeSummary> {
    let mut grouped = BTreeMap::<String, (u32, u64, u64)>::new();
    for metrics in metrics {
        for profile in &metrics.stage_profiles {
            grouped
                .entry(profile.stage.clone())
                .and_modify(|(runs, total_us, max_us)| {
                    *runs += 1;
                    *total_us += profile.duration_us;
                    *max_us = (*max_us).max(profile.duration_us);
                })
                .or_insert((1, profile.duration_us, profile.duration_us));
        }
    }

    grouped
        .into_iter()
        .map(|(stage, (runs, total_us, max_us))| StageTimeSummary {
            stage,
            runs,
            total_duration_ms: total_us as f32 / 1_000.0,
            avg_duration_ms: if runs == 0 {
                0.0
            } else {
                total_us as f32 / 1_000.0 / runs as f32
            },
            max_duration_ms: max_us as f32 / 1_000.0,
        })
        .collect()
}

fn ratio(count: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        count as f32 / total as f32
    }
}

fn write_fixture_repo(repo: &Path, task: &BenchTask) -> KimetsuResult<()> {
    fs::create_dir_all(repo)?;
    fs::write(
        repo.join("README.md"),
        format!(
            "# {}\n\nSeed task: {}\nFollow-up task: {}\n",
            task.id, task.seed_task, task.followup_task
        ),
    )?;

    for file in &task.files {
        let path = repo.join(file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, file.content)?;
    }
    Ok(())
}

fn render_report(report: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Kimetsu Bench {}\n\n", report.bench_run_id));
    out.push_str(&format!("Tasks: {}\n\n", report.task_count));
    out.push_str("This Phase 6 slice benchmarks context and memory retrieval, then runs deterministic dry-run PatchPlan traces for brain_on_cold and brain_on_warm with the model disabled. Implementation edits and verification commands are not executed yet.\n\n");
    out.push_str("## Summary\n\n");
    out.push_str("| mode | success | relevant_signal | memories | context | irrelevant_context | dry_runs | avg_ms | trace_events | model_turns | model_skips | planned_relevant | unrelated_planned |\n");
    out.push_str(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for summary in &report.summaries {
        out.push_str(&format!(
            "| {} | {:.0}% | {:.0}% | {} | {} | {} | {} | {:.2} | {} | {} | {} | {} | {} |\n",
            summary.mode,
            summary.success_rate * 100.0,
            summary.relevant_signal_rate * 100.0,
            summary.accepted_memories_used,
            summary.context_loads,
            summary.irrelevant_context_loaded,
            summary.dry_runs,
            summary.avg_duration_ms,
            summary.trace_events,
            summary.model_turns,
            summary.model_skips,
            summary.planned_relevant_files,
            summary.unrelated_planned_files,
        ));
    }

    out.push_str("\n## Tasks\n\n");
    out.push_str("| task | category | mode | success | memories | context | irrelevant | run | duration_ms | trace_events | planned_relevant | unrelated_planned |\n");
    out.push_str(
        "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for result in &report.results {
        let dry_run = result.dry_run.as_ref();
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {:.2} | {} | {} | {} |\n",
            result.task_id,
            result.category,
            result.mode,
            if result.success { "yes" } else { "no" },
            result.accepted_memories_used,
            result.context_loads,
            result.irrelevant_context_loaded,
            dry_run
                .map(|metrics| metrics.terminal_kind.as_str())
                .unwrap_or("not_run"),
            dry_run
                .map(|metrics| metrics.duration_us as f32 / 1_000.0)
                .unwrap_or(0.0),
            dry_run.map(|metrics| metrics.trace_events).unwrap_or(0),
            dry_run
                .map(|metrics| metrics.planned_relevant_files)
                .unwrap_or(0),
            dry_run
                .map(|metrics| metrics.unrelated_planned_files)
                .unwrap_or(0),
        ));
    }

    out.push_str("\n## Stage Time Profile\n\n");
    out.push_str("Durations are derived from trace `stage.entered` and `stage.completed` event timestamps.\n\n");
    out.push_str("| mode | stage | runs | avg_ms | max_ms | total_ms |\n");
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: |\n");
    for summary in &report.summaries {
        for profile in &summary.stage_profiles {
            out.push_str(&format!(
                "| {} | {} | {} | {:.2} | {:.2} | {:.2} |\n",
                summary.mode,
                profile.stage,
                profile.runs,
                profile.avg_duration_ms,
                profile.max_duration_ms,
                profile.total_duration_ms,
            ));
        }
    }

    out
}

fn bench_tasks() -> Vec<BenchTask> {
    vec![
        BenchTask {
            id: "rust_retry_budget",
            category: "rust",
            seed_task: "Add retry budget handling to the verification failure loop.",
            followup_task: "Stop when the same failure fingerprint repeats twice.",
            memory_kind: MemoryKind::FailurePattern,
            warm_memory: "When the same failure fingerprint repeats twice, update src/retry.rs; the retry budget and normalized_first_error_line logic live there.",
            relevant_handles: vec!["file:src/retry.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"bench\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                file(
                    "src/retry.rs",
                    "pub struct FailureFingerprint { pub exit_code: i32, pub normalized_first_error_line: String }\n\npub fn should_stop_after_repeat(seen: usize) -> bool { seen >= 2 }\n",
                ),
                file(
                    "src/projector.rs",
                    "pub fn rebuild_projection() { /* event replay schema version */ }\n",
                ),
                file(
                    "src/shell.rs",
                    "pub fn command_timeout_policy() { /* shell timeout and kill tree */ }\n",
                ),
            ],
        },
        BenchTask {
            id: "rust_capsule_scoring",
            category: "rust",
            seed_task: "Tune context capsule scoring for stage-aware retrieval.",
            followup_task: "Adjust patch_plan capsule scoring so conventions outrank freshness.",
            memory_kind: MemoryKind::Convention,
            warm_memory: "For patch_plan capsule scoring and conventions outranking freshness, edit src/context.rs weights_for_stage and normalize_and_score.",
            relevant_handles: vec!["file:src/context.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"bench\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                file(
                    "src/context.rs",
                    "pub fn normalize_and_score() { /* relevance confidence freshness scope */ }\npub fn weights_for_stage(stage: &str) { let _ = stage; }\n",
                ),
                file(
                    "src/ingest.rs",
                    "pub fn repo_file_candidates() { /* file snippets manifests */ }\n",
                ),
                file(
                    "src/memory.rs",
                    "pub fn normalize_memory_text() { /* lowercase whitespace collapse */ }\n",
                ),
            ],
        },
        BenchTask {
            id: "rust_secret_redaction",
            category: "rust",
            seed_task: "Redact secrets from shell artifacts.",
            followup_task: "Make model trace artifacts avoid leaking API keys.",
            memory_kind: MemoryKind::Convention,
            warm_memory: "API key redaction and model trace artifact scrubbing belong in src/redaction.rs before artifacts are written.",
            relevant_handles: vec!["file:src/redaction.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"bench\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                file(
                    "src/redaction.rs",
                    "pub fn redact_text(value: &str) -> String { value.replace(\"api-secret\", \"[redacted]\") }\n",
                ),
                file(
                    "src/anthropic.rs",
                    "pub fn send_messages() { /* x-api-key header never enters trace */ }\n",
                ),
                file(
                    "src/report.rs",
                    "pub fn render_final_report() { /* markdown report */ }\n",
                ),
            ],
        },
        BenchTask {
            id: "rust_project_lock",
            category: "rust",
            seed_task: "Add single-writer project lock handling.",
            followup_task: "Recover stale project locks after an interrupted run.",
            memory_kind: MemoryKind::FailurePattern,
            warm_memory: "Stale project lock recovery is implemented in src/lock.rs around project.lock payload pid, run_id, and started_at.",
            relevant_handles: vec!["file:src/lock.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"bench\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                file(
                    "src/lock.rs",
                    "pub struct ProjectLock { pub pid: u32, pub run_id: Option<String> }\npub fn clear_stale_lock() {}\n",
                ),
                file(
                    "src/trace.rs",
                    "pub fn append_trace_line() { /* fsync jsonl */ }\n",
                ),
                file(
                    "src/project.rs",
                    "pub fn init_project() { /* project toml */ }\n",
                ),
            ],
        },
        BenchTask {
            id: "rust_manifest_ingest",
            category: "rust",
            seed_task: "Index repo manifests during ingestion.",
            followup_task: "Add Cargo manifest summaries to context retrieval.",
            memory_kind: MemoryKind::Fact,
            warm_memory: "Cargo manifest summaries are parsed and stored from src/ingest.rs manifest_record during repo ingestion.",
            relevant_handles: vec!["file:src/ingest.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"bench\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                file(
                    "src/ingest.rs",
                    "pub fn manifest_record(path: &str) { /* Cargo.toml package.json pyproject go.mod */ }\n",
                ),
                file("src/schema.rs", "pub fn create_repo_manifests_table() {}\n"),
                file("src/context.rs", "pub fn manifest_candidates() {}\n"),
            ],
        },
        BenchTask {
            id: "ts_debounce",
            category: "typescript",
            seed_task: "Fix repeated calls in the debounce helper.",
            followup_task: "Make debounce flush cancel pending timers before invoking.",
            memory_kind: MemoryKind::Fact,
            warm_memory: "Debounce flush and cancel pending timers are in src/debounce.ts; tests should cover invoke, cancel, and flush.",
            relevant_handles: vec!["file:src/debounce.ts"],
            files: vec![
                file(
                    "package.json",
                    "{\"scripts\":{\"test\":\"vitest run\"},\"dependencies\":{}}\n",
                ),
                file(
                    "src/debounce.ts",
                    "export function debounce(fn: Function, delay: number) { let timer: any; return { call(){ clearTimeout(timer); timer = setTimeout(fn, delay); }, flush(){ clearTimeout(timer); fn(); } }; }\n",
                ),
                file("src/routes.ts", "export const routes = [];\n"),
                file(
                    "src/json.ts",
                    "export function parseJson(value: string) { return JSON.parse(value); }\n",
                ),
            ],
        },
        BenchTask {
            id: "ts_route_config",
            category: "typescript",
            seed_task: "Load route config from a typed registry.",
            followup_task: "Prevent duplicate route ids in the route registry.",
            memory_kind: MemoryKind::Convention,
            warm_memory: "Route registry duplicate id validation belongs in src/routes.ts, not in the JSON parser or debounce helper.",
            relevant_handles: vec!["file:src/routes.ts"],
            files: vec![
                file(
                    "package.json",
                    "{\"scripts\":{\"test\":\"vitest run\"},\"dependencies\":{}}\n",
                ),
                file(
                    "src/routes.ts",
                    "export type Route = { id: string; path: string };\nexport function validateRoutes(routes: Route[]) { return new Set(routes.map(route => route.id)).size === routes.length; }\n",
                ),
                file(
                    "src/config.ts",
                    "export function loadConfig() { return {}; }\n",
                ),
                file("src/debounce.ts", "export function debounce() {}\n"),
            ],
        },
        BenchTask {
            id: "ts_json_parse",
            category: "typescript",
            seed_task: "Return structured errors from JSON parsing.",
            followup_task: "Include the JSON parse error offset in diagnostics.",
            memory_kind: MemoryKind::Fact,
            warm_memory: "JSON parse diagnostics and error offsets live in src/json.ts, specifically parseJsonWithDiagnostics.",
            relevant_handles: vec!["file:src/json.ts"],
            files: vec![
                file(
                    "package.json",
                    "{\"scripts\":{\"test\":\"vitest run\"},\"dependencies\":{}}\n",
                ),
                file(
                    "src/json.ts",
                    "export function parseJsonWithDiagnostics(value: string) { try { return { ok: true, value: JSON.parse(value) }; } catch (error) { return { ok: false, error }; } }\n",
                ),
                file("src/routes.ts", "export const routes = [];\n"),
                file(
                    "src/config.ts",
                    "export function readEnv() { return process.env; }\n",
                ),
            ],
        },
        BenchTask {
            id: "memory_rg_preference",
            category: "memory",
            seed_task: "Remember the preferred code search command.",
            followup_task: "For code search, which command should the agent prefer?",
            memory_kind: MemoryKind::Preference,
            warm_memory: "For code search, the user prefers rg over grep; use rg or rg --files before slower alternatives.",
            relevant_handles: vec!["file:docs/workflows.md"],
            files: vec![
                file(
                    "docs/workflows.md",
                    "Code search workflow: prefer rg for fast repository search, and use rg --files for filename discovery.\n",
                ),
                file(
                    "docs/shell.md",
                    "Commands are executed directly as program plus args.\n",
                ),
                file(
                    "src/search.rs",
                    "pub fn search_files() { /* ripgrep-like search abstraction */ }\n",
                ),
            ],
        },
        BenchTask {
            id: "memory_windows_shell",
            category: "memory",
            seed_task: "Remember Windows shell safety rules.",
            followup_task: "On Windows, how should recursive delete commands be handled?",
            memory_kind: MemoryKind::Preference,
            warm_memory: "On Windows, avoid string-built shell deletion; verify resolved paths and use native PowerShell Remove-Item with LiteralPath.",
            relevant_handles: vec!["file:docs/windows.md"],
            files: vec![
                file(
                    "docs/windows.md",
                    "Windows shell policy: verify paths before recursive delete, prefer native PowerShell cmdlets, and avoid string-built shell commands.\n",
                ),
                file(
                    "docs/unix.md",
                    "Unix shell policy: spawn commands directly without raw shell strings.\n",
                ),
                file("src/shell.rs", "pub fn validate_delete_policy() {}\n"),
            ],
        },
        BenchTask {
            id: "context_mvp_benchmark",
            category: "context",
            seed_task: "Document the internal MVP benchmark.",
            followup_task: "Where is the brain_on_warm benchmark mode defined?",
            memory_kind: MemoryKind::Fact,
            warm_memory: "The brain_on_warm benchmark mode is defined in docs/bench.md with paired seed and follow-up tasks.",
            relevant_handles: vec!["file:docs/bench.md"],
            files: vec![
                file(
                    "docs/bench.md",
                    "Benchmark modes: brain_off, brain_on_cold, brain_on_warm. Warm mode is pre-seeded with relevant memories from paired seed tasks.\n",
                ),
                file(
                    "docs/mvp.md",
                    "MVP benchmark compares warm follow-up tasks against cold runs.\n",
                ),
                file("src/context.rs", "pub fn retrieve_context() {}\n"),
            ],
        },
        BenchTask {
            id: "context_skip_dirs",
            category: "context",
            seed_task: "Document repo ingestion skip dirs.",
            followup_task: "Which ingestion document explains skipping target and node_modules?",
            memory_kind: MemoryKind::Fact,
            warm_memory: "Skipping target and node_modules during ingestion is documented in docs/ingestion.md and implemented by skip_dirs.",
            relevant_handles: vec!["file:docs/ingestion.md"],
            files: vec![
                file(
                    "docs/ingestion.md",
                    "Repo ingestion skip dirs include .git, .kimetsu, node_modules, target, dist, build, vendor, and virtualenv caches.\n",
                ),
                file(
                    "docs/context.md",
                    "Context retrieval packs capsules by token budget and score.\n",
                ),
                file("src/ingest.rs", "pub fn skip_dirs() {}\n"),
            ],
        },
    ]
}

fn file(path: &'static str, content: &'static str) -> FixtureFile {
    FixtureFile { path, content }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_reports_warm_memory_reuse() {
        let repo = std::env::temp_dir().join(format!("kimetsu-bench-report-{}", new_id()));
        fs::create_dir_all(&repo).expect("create repo");

        let report = run_benchmark(BenchOptions {
            repo: repo.clone(),
            keep_fixtures: false,
        })
        .expect("run benchmark");

        assert_eq!(report.task_count, 12);
        assert!(report.report_path.exists());
        assert!(report.results_path.exists());
        let warm = report
            .summaries
            .iter()
            .find(|summary| summary.mode == "brain_on_warm")
            .expect("warm summary");
        assert!(warm.accepted_memories_used >= report.task_count as u32);
        assert!(!warm.stage_profiles.is_empty());
        assert!(
            warm.stage_profiles
                .iter()
                .any(|profile| profile.stage == "repo_scan")
        );

        fs::remove_dir_all(repo).expect("cleanup");
    }
}
