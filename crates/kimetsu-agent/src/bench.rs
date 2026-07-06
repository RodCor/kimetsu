use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_brain::context::ContextCapsule;
use kimetsu_brain::project;
use kimetsu_brain::trace::read_trace;
use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::ids::new_id;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use kimetsu_core::paths::{ProjectPaths, user_cache_dir_for};
use serde::{Deserialize, Serialize};

use crate::pipeline::{CodingRunOptions, PatchPlan, run_coding_dry_run};

const MIN_MODEL_BENCH_REMAINING_USD: f32 = 0.01;

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub repo: PathBuf,
    pub keep_fixtures: bool,
    pub model_backed: bool,
    pub limit: Option<usize>,
    pub max_cost_usd: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRunResult {
    pub bench_run_id: String,
    pub task_count: usize,
    pub model_backed: bool,
    pub total_cost_usd: f32,
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
    pub invalid_planned_files: u32,
    pub avg_patch_plan_quality: f32,
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
    model_backed: bool,
    max_cost_usd: f32,
    total_cost_usd: f32,
    stopped_reason: Option<String>,
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
    /// MP-1.6: only populated for OnAutoWarm. Lists every memory the
    /// seed phase auto-accepted into the project's brain so the followup
    /// phase's regression is auditable against the exact text injected.
    #[serde(default)]
    auto_accepted_memories: Vec<AutoAcceptedMemory>,
    /// MP-1.6: only populated for OnAutoWarm. Path to the saved
    /// seed-phase trace.jsonl (under the bench artifact dir) so triage does
    /// not require keeping the temp fixture alive.
    #[serde(default)]
    seed_trace_artifact: Option<String>,
    /// MP-1.6: capsules that the broker actually surfaced into the followup
    /// phase, with kind/handle/score/relevance. Lets the report distinguish
    /// "memories did the work" from "prior_run did the work".
    #[serde(default)]
    injected_capsules: Vec<InjectedCapsuleSnapshot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DryRunBenchMetrics {
    run_id: String,
    trace_artifact: String,
    patch_plan_artifact: String,
    terminal_kind: String,
    /// MP-1.7 polish: when terminal_kind is run.failed, pull the `category`
    /// field from the run.failed payload so the report can distinguish a
    /// graceful early-exit (`Gate`, e.g. the plan-create existence guard)
    /// from a real crash (`Implementation`, `Verification`, `Provider`).
    #[serde(default)]
    failure_category: Option<String>,
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
    invalid_planned_files: u32,
    patch_plan_quality_score: f32,
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
    /// When true the bench drives only PatchPlan; when false it runs the full
    /// pipeline including Implementation and Verification. Real-fix fixtures
    /// must set this to false; retrieval-only fixtures keep the default.
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct FixtureFile {
    path: &'static str,
    content: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct PatchPlanQuality {
    invalid_planned_files: u32,
    score: f32,
}

#[derive(Debug, Clone)]
struct ModelBenchConfig {
    config: ProjectConfig,
    model_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchMode {
    Off,
    OnCold,
    OnWarm,
    /// MP-3: run the seed pipeline first, auto-accept high-confidence personal
    /// proposals from its trace, reset the source files, then run the
    /// follow-up pipeline. Memories carried across phases are *only* what the
    /// agent itself proposed -- no hand-curated warm seed.
    OnAutoWarm,
    /// MP-1.7 ablation: same flow as OnAutoWarm, but after auto-accept
    /// the bench wipes the `memories` projection so the followup phase
    /// surfaces only repo_file / repo_manifest / prior_run capsules. Lets
    /// the report quantify how much of auto_warm's win is memory vs
    /// prior-run trace persistence.
    OnAutoWarmNoMemory,
}

impl BenchMode {
    fn all() -> [Self; 5] {
        [
            Self::Off,
            Self::OnCold,
            Self::OnWarm,
            Self::OnAutoWarm,
            Self::OnAutoWarmNoMemory,
        ]
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "brain_off",
            Self::OnCold => "brain_on_cold",
            Self::OnWarm => "brain_on_warm",
            Self::OnAutoWarm => "brain_on_auto_warm",
            Self::OnAutoWarmNoMemory => "brain_on_auto_warm_no_memory",
        }
    }
}

/// Print binary path, build mtime, and crate version on stderr at the start
/// of every bench run. Lesson learned from the MP-1.6 cycle: launching a
/// model-backed bench with a stale `target/debug/kimetsu` cost ~30 minutes
/// and ~$8 because the new instrumentation simply was not in the binary.
/// One line of stdout makes that mistake impossible to miss.
fn print_bench_provenance() {
    let exe_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let mtime_iso = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| {
            time::OffsetDateTime::from(t)
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "<unknown>".to_string());
    eprintln!(
        "kimetsu_bench provenance: version={} binary={} built_at={}",
        env!("CARGO_PKG_VERSION"),
        exe_path,
        mtime_iso,
    );
}

pub fn run_benchmark(options: BenchOptions) -> KimetsuResult<BenchRunResult> {
    print_bench_provenance();
    let bench_run_id = new_id().to_string();
    let mut tasks = bench_tasks();
    if let Some(limit) = options.limit {
        tasks.truncate(limit.min(tasks.len()));
    }
    let model_config = if options.model_backed {
        Some(load_model_bench_config(&options)?)
    } else {
        None
    };
    let repo_canonical = options.repo.canonicalize().unwrap_or(options.repo.clone());
    let output_dir = user_cache_dir_for(&repo_canonical)
        .join("bench")
        .join(&bench_run_id);
    fs::create_dir_all(&output_dir)?;

    let fixture_root = std::env::temp_dir().join(format!("kimetsu-bench-{bench_run_id}"));
    if fixture_root.exists() {
        fs::remove_dir_all(&fixture_root)?;
    }
    fs::create_dir_all(&fixture_root)?;

    let mut results = Vec::new();
    let mut total_cost_usd = 0.0_f32;
    let mut stopped_reason = None;
    let run_result = (|| -> KimetsuResult<()> {
        'tasks: for task in &tasks {
            for mode in BenchMode::all() {
                let remaining_cost_usd = if model_config.is_some() {
                    (options.max_cost_usd - total_cost_usd).max(0.0)
                } else {
                    options.max_cost_usd
                };
                if model_config.is_some() && remaining_cost_usd < MIN_MODEL_BENCH_REMAINING_USD {
                    stopped_reason = Some(format!(
                        "stopped before {} {} because remaining budget ${:.4} is below ${:.2}",
                        task.id,
                        mode.as_str(),
                        remaining_cost_usd,
                        MIN_MODEL_BENCH_REMAINING_USD
                    ));
                    break 'tasks;
                }
                let result = run_task_mode(
                    &fixture_root,
                    &output_dir,
                    task,
                    mode,
                    model_config.as_ref(),
                    remaining_cost_usd,
                )?;
                if let Some(metrics) = result.dry_run.as_ref() {
                    total_cost_usd += metrics.total_cost_usd;
                }
                results.push(result);
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

    total_cost_usd = clean_zero(total_cost_usd);
    let summaries = summarize_results(&results);
    let report = BenchReport {
        bench_run_id: bench_run_id.clone(),
        task_count: tasks.len(),
        model_backed: options.model_backed,
        max_cost_usd: options.max_cost_usd,
        total_cost_usd,
        stopped_reason,
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
        model_backed: options.model_backed,
        total_cost_usd,
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
    model_config: Option<&ModelBenchConfig>,
    remaining_cost_usd: f32,
) -> KimetsuResult<BenchTaskResult> {
    let repo = fixture_root.join(format!("{}-{}", task.id, mode.as_str()));
    write_fixture_repo(&repo, task)?;
    // Make each fixture its own git boundary so its brain initializes
    // inside the fixture, never climbing to an enclosing repo (e.g. a
    // dev's $HOME repo) and leaking fixture memories there.
    kimetsu_core::paths::git_init_boundary(&repo);
    project::init_project(&repo, false)?;
    if let Some(model_config) = model_config {
        configure_fixture_model(&repo, model_config, remaining_cost_usd)?;
    }
    if mode == BenchMode::OnWarm {
        project::add_memory(
            &repo,
            MemoryScope::Repo,
            task.memory_kind,
            &format!("[bench:{}] {}", task.id, task.warm_memory),
        )?;
    }
    project::ingest_repo(&repo)?;

    // Two-phase auto-warm modes: run the *seed_task* (intentionally different
    // from the followup so the seed phase produces transferable conventions,
    // not a dress rehearsal of the same fix), auto-accept any high-confidence
    // personal proposals it produced, reset the source files, then run the
    // followup task. The seed-phase trace is preserved as an artifact so the
    // report can audit which memories carried over.
    //
    // MP-1.7 ablation: `OnAutoWarmNoMemory` does the same flow but
    // wipes the `memories` projection between seed and followup. The
    // run.finished events from the seed phase stay in the canonical trace,
    // so the broker can still surface a `prior_run` capsule -- only memory
    // capsules disappear. The delta between AutoWarm and AutoWarmNoMemory
    // isolates the actual contribution of accepted memories.
    let mut auto_accepted: Vec<AutoAcceptedMemory> = Vec::new();
    let mut seed_trace_artifact: Option<String> = None;
    if matches!(mode, BenchMode::OnAutoWarm | BenchMode::OnAutoWarmNoMemory) {
        let seed_result = run_pipeline_phase(&repo, task, task.seed_task, mode, model_config)?;
        seed_trace_artifact = Some(copy_seed_trace_artifact(
            output_dir,
            task,
            mode,
            &seed_result.trace_path,
        )?);
        auto_accepted = auto_accept_pending_proposals(&repo)?;
        if mode == BenchMode::OnAutoWarmNoMemory {
            wipe_memory_projection(&repo)?;
        }
        // Reset source files so the followup phase has a fresh surface.
        write_fixture_repo(&repo, task)?;
        project::ingest_repo(&repo)?;
    }

    let measured_capsules = if mode == BenchMode::Off {
        Vec::new()
    } else {
        project::retrieve_context(&repo, "patch_plan", task.followup_task, 420)?.capsules
    };
    let injected_capsules: Vec<InjectedCapsuleSnapshot> = measured_capsules
        .iter()
        .map(|c| InjectedCapsuleSnapshot {
            kind: c.kind.clone(),
            handle: c.expansion_handle.clone(),
            score: c.score,
            relevance: c.relevance,
        })
        .collect();
    let pipeline_result = run_pipeline_phase(&repo, task, task.followup_task, mode, model_config)?;
    let metrics = dry_run_metrics(output_dir, task, mode, &pipeline_result.trace_path)?;
    let mut result = evaluate_capsules(task, mode, measured_capsules, Some(metrics));
    result.injected_capsules = injected_capsules;
    if matches!(mode, BenchMode::OnAutoWarm | BenchMode::OnAutoWarmNoMemory) {
        // Surface the memory text and IDs the seed phase accepted, plus the
        // seed-phase trace path. For OnAutoWarmNoMemory we still record
        // what would have been carried over so the report shows what the
        // ablation is actually withholding.
        result.accepted_memories_used = result
            .accepted_memories_used
            .max(auto_accepted.len() as u32);
        result.auto_accepted_memories = auto_accepted;
        result.seed_trace_artifact = seed_trace_artifact;
    }
    Ok(result)
}

/// MP-1.7: drop every row from the `memories` projection so the broker
/// surfaces no `memory:` capsules in the followup retrieval. The canonical
/// trace (run.started / memory.accepted / run.finished events) is preserved;
/// only the brain.db projection is cleared. This is the cheapest way to
/// isolate "what did the accepted memories contribute?" from "what did the
/// prior_run capsule contribute?" without touching the broker code.
fn wipe_memory_projection(repo: &Path) -> KimetsuResult<()> {
    let (_paths, _config, conn) = project::load_project(repo)?;
    conn.execute_batch(
        "
        DELETE FROM memories;
        DELETE FROM memories_fts;
        ",
    )?;
    Ok(())
}

fn copy_seed_trace_artifact(
    output_dir: &Path,
    task: &BenchTask,
    mode: BenchMode,
    trace_path: &Path,
) -> KimetsuResult<String> {
    let artifact_dir = output_dir
        .join("artifacts")
        .join(task.id)
        .join(mode.as_str());
    fs::create_dir_all(&artifact_dir)?;
    let dest = artifact_dir.join("seed_trace.jsonl");
    fs::copy(trace_path, &dest)?;
    Ok(format!(
        "artifacts/{}/{}/seed_trace.jsonl",
        task.id,
        mode.as_str()
    ))
}

/// Run a single pipeline phase. On a hard error the trace was still finalized
/// by `run_coding`, so we recover the most recent run directory and let the
/// metric layer parse what it can.
fn run_pipeline_phase(
    repo: &Path,
    task: &BenchTask,
    task_text: &str,
    mode: BenchMode,
    model_config: Option<&ModelBenchConfig>,
) -> KimetsuResult<crate::pipeline::CodingRunResult> {
    let pipeline_result = run_coding_dry_run(CodingRunOptions {
        repo: repo.to_path_buf(),
        task: task_text.to_string(),
        dry_run: task.dry_run,
        allow_high_risk: true,
        disable_model: model_config.is_none(),
        disable_broker: mode == BenchMode::Off,
        model_key_override: model_config.map(|model| model.model_key.clone()),
    });
    match pipeline_result {
        Ok(result) => Ok(result),
        Err(err) => {
            let kimetsu_dir = repo.join(".kimetsu");
            let runs_dir = kimetsu_dir.join("runs");
            let last_run = fs::read_dir(&runs_dir).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .max_by_key(|path| path.metadata().and_then(|m| m.modified()).ok())
            });
            let Some(run_dir) = last_run else {
                return Err(err);
            };
            let trace_path = run_dir.join("trace.jsonl");
            if !trace_path.exists() {
                return Err(err);
            }
            Ok(crate::pipeline::CodingRunResult {
                run_id: kimetsu_core::ids::RunId::new(),
                dry_run: task.dry_run,
                patch_plan_id: String::new(),
                final_report_path: run_dir.join("final_report.md"),
                trace_path,
            })
        }
    }
}

/// Apply the docs/MEMORY-PROPOSALS.md auto-accept heuristics to whatever pending
/// proposals are sitting in the project's brain.db, accepting those that pass
/// and ignoring the rest. Returns the accepted proposals so the bench can
/// surface their text in the report (per Codex review: measurement integrity
/// requires knowing exactly which memory the seed phase carried into the
/// followup, not just a count).
fn auto_accept_pending_proposals(repo: &Path) -> KimetsuResult<Vec<AutoAcceptedMemory>> {
    let pending = project::list_proposals(
        repo,
        project::ProposalFilter {
            status: Some("pending".into()),
            limit: 100,
            ..project::ProposalFilter::default()
        },
    )?;
    // MP-4c: load the shadow pool once per auto-accept pass. The policy
    // rejects re-acceptance of any proposal whose normalized_text overlaps
    // an existing memory with use_count >= 3 and a usefulness ratio below
    // -0.2.
    let shadow_pool = build_shadow_pool(repo)?;
    let mut accepted = Vec::new();
    for proposal in pending {
        if !auto_accept_policy_allows(&proposal, &shadow_pool) {
            continue;
        }
        match project::accept_proposal(
            repo,
            &proposal.proposal_id,
            project::AcceptOverrides::default(),
        ) {
            Ok(memory_id) => accepted.push(AutoAcceptedMemory {
                memory_id,
                proposal_id: proposal.proposal_id,
                scope: proposal.scope,
                kind: proposal.kind,
                text: proposal.text,
                confidence: proposal.proposed_confidence,
            }),
            Err(_) => continue,
        }
    }
    Ok(accepted)
}

#[derive(Debug, Clone)]
pub(crate) struct ProposalShadow {
    pub scope: String,
    pub kind: String,
    pub normalized_text: String,
    pub use_count: u32,
    pub usefulness_score: f32,
}

fn build_shadow_pool(repo: &Path) -> KimetsuResult<Vec<ProposalShadow>> {
    let memories = project::list_memories(repo)?;
    Ok(memories
        .into_iter()
        .map(|m| ProposalShadow {
            scope: m.scope,
            kind: m.kind,
            normalized_text: kimetsu_core::memory::normalize_memory_text(&m.text),
            use_count: m.use_count,
            usefulness_score: m.usefulness_score,
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoAcceptedMemory {
    pub memory_id: String,
    pub proposal_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectedCapsuleSnapshot {
    pub kind: String,
    pub handle: String,
    pub score: f32,
    pub relevance: f32,
}

/// Auto-accept policy. failure_pattern always requires human review;
/// preference auto-accepts only at global_user with high confidence;
/// convention auto-accepts at repo or project with mid-confidence.
///
/// MP-4c adds a shadowing check: a proposal that re-states a low-usefulness
/// existing memory (high token-Jaccard overlap, same scope+kind,
/// use_count >= 3, usefulness ratio < -0.2) is rejected even if it would
/// otherwise pass on confidence. Lets the brain stop re-injecting the same
/// hurting convention every seed run.
///
/// History:
/// - MP-3 originals: preference 0.8, convention 0.7.
/// - MP-1.5 lowered to preference 0.75, convention 0.6 to let real
///   proposals through.
/// - MP-1.6 reverts convention to 0.7 after the second bench showed two
///   real-fix tasks regressing because mid-confidence conventions were
///   applied where they did not fit.
/// - MP-4c adds shadowing against low-usefulness existing memories.
pub(crate) fn auto_accept_policy_allows(
    proposal: &project::ProposalRow,
    shadow_pool: &[ProposalShadow],
) -> bool {
    let kind = proposal.kind.to_lowercase();
    let scope = proposal.scope.to_lowercase();
    let conf = proposal.proposed_confidence;
    if proposal.text.trim().is_empty() {
        return false;
    }
    let passes_confidence = match (kind.as_str(), scope.as_str()) {
        ("preference", "global_user") => conf >= 0.75,
        ("convention", "repo") | ("convention", "project") => conf >= 0.7,
        _ => false,
    };
    if !passes_confidence {
        return false;
    }
    if is_shadowed_by_low_usefulness(proposal, shadow_pool) {
        return false;
    }
    true
}

const SHADOW_JACCARD_THRESHOLD: f32 = 0.5;
const SHADOW_MIN_USE_COUNT: u32 = 3;
const SHADOW_MAX_RATIO: f32 = -0.2;

fn is_shadowed_by_low_usefulness(
    proposal: &project::ProposalRow,
    shadow_pool: &[ProposalShadow],
) -> bool {
    let proposal_normalized = kimetsu_core::memory::normalize_memory_text(&proposal.text);
    let proposal_tokens = jaccard_tokens(&proposal_normalized);
    if proposal_tokens.is_empty() {
        return false;
    }
    let proposal_scope = proposal.scope.to_lowercase();
    let proposal_kind = proposal.kind.to_lowercase();
    for shadow in shadow_pool {
        if shadow.use_count < SHADOW_MIN_USE_COUNT {
            continue;
        }
        let ratio = shadow.usefulness_score / shadow.use_count as f32;
        if ratio >= SHADOW_MAX_RATIO {
            continue;
        }
        if shadow.scope.to_lowercase() != proposal_scope {
            continue;
        }
        if shadow.kind.to_lowercase() != proposal_kind {
            continue;
        }
        let overlap = jaccard_similarity(&proposal_tokens, &shadow.normalized_text);
        if overlap >= SHADOW_JACCARD_THRESHOLD {
            return true;
        }
    }
    false
}

fn jaccard_tokens(text: &str) -> std::collections::BTreeSet<String> {
    text.split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn jaccard_similarity(a: &std::collections::BTreeSet<String>, b_text: &str) -> f32 {
    let b: std::collections::BTreeSet<String> = jaccard_tokens(b_text);
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(&b).count() as f32;
    let union = a.union(&b).count() as f32;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn load_model_bench_config(options: &BenchOptions) -> KimetsuResult<ModelBenchConfig> {
    if options.max_cost_usd < MIN_MODEL_BENCH_REMAINING_USD {
        return Err(format!(
            "--max-cost-usd must be at least {:.2} for --model-backed",
            MIN_MODEL_BENCH_REMAINING_USD
        )
        .into());
    }

    let (paths, config, _conn) = project::load_project(&options.repo)?;
    if !matches!(config.model.provider.as_str(), "anthropic" | "claude_code") {
        return Err(format!(
            "unsupported model provider for benchmark: {}",
            config.model.provider
        )
        .into());
    }

    let Some(model_key) = resolve_env_value(&paths.repo_root, &config.model.api_key_env) else {
        return Err(format!(
            "model-backed benchmark requires {} in the environment or source repo .env",
            config.model.api_key_env
        )
        .into());
    };

    Ok(ModelBenchConfig { config, model_key })
}

fn configure_fixture_model(
    repo: &Path,
    model_config: &ModelBenchConfig,
    remaining_cost_usd: f32,
) -> KimetsuResult<()> {
    let paths = ProjectPaths::discover(repo)?;
    let mut config = project::load_config(&paths)?;
    config.model = model_config.config.model.clone();
    config.run.max_total_cost_usd = model_config
        .config
        .run
        .max_total_cost_usd
        .min(remaining_cost_usd);
    fs::write(&paths.project_toml, config.to_toml()?)?;
    Ok(())
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

    // For real-fix tasks (`task.dry_run == false`) success means the pipeline
    // reached `run.finished`, which implies the Verification gate passed all
    // declared verification commands. For retrieval-only tasks we keep the
    // legacy "did relevant capsules load" definition so existing fixtures stay
    // comparable across modes.
    let success = if task.dry_run {
        relevant_signal_loaded
    } else {
        dry_run
            .as_ref()
            .map(|metrics| metrics.terminal_kind == "run.finished")
            .unwrap_or(false)
    };

    BenchTaskResult {
        task_id: task.id.to_string(),
        category: task.category.to_string(),
        mode: mode.as_str().to_string(),
        success,
        relevant_signal_loaded,
        relevant_files_loaded,
        accepted_memories_used,
        context_loads: included_handles.len() as u32,
        irrelevant_context_loaded,
        included_handles,
        dry_run,
        auto_accepted_memories: Vec::new(),
        seed_trace_artifact: None,
        injected_capsules: Vec::new(),
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
    let terminal_event = events.iter().rev().find(|event| {
        matches!(
            event.kind.as_str(),
            "run.finished" | "run.failed" | "run.aborted"
        )
    });
    let terminal_kind = terminal_event
        .map(|event| event.kind.clone())
        .unwrap_or_else(|| "running".to_string());
    let failure_category = terminal_event
        .filter(|event| event.kind == "run.failed")
        .and_then(|event| event.payload.get("category").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
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
    let quality = patch_plan_quality(
        task,
        &patch_plan,
        planned_relevant_files,
        unrelated_planned_files,
    );

    Ok(DryRunBenchMetrics {
        run_id,
        trace_artifact,
        patch_plan_artifact,
        terminal_kind,
        failure_category,
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
        invalid_planned_files: quality.invalid_planned_files,
        patch_plan_quality_score: quality.score,
        risk_level: format!("{:?}", patch_plan.risk_level).to_ascii_lowercase(),
    })
}

fn patch_plan_quality(
    task: &BenchTask,
    patch_plan: &PatchPlan,
    planned_relevant_files: u32,
    unrelated_planned_files: u32,
) -> PatchPlanQuality {
    let mut fixture_paths = task.files.iter().map(|file| file.path).collect::<Vec<_>>();
    fixture_paths.push("README.md");
    let invalid_existing = patch_plan
        .files_to_read
        .iter()
        .chain(patch_plan.files_to_modify.iter())
        .chain(patch_plan.files_to_delete.iter())
        .filter(|planned| !fixture_paths.iter().any(|path| path == &planned.as_str()))
        .count() as u32;
    let invalid_creates = patch_plan
        .files_to_create
        .iter()
        .filter(|planned| fixture_paths.iter().any(|path| path == &planned.as_str()))
        .count() as u32;
    let invalid_planned_files = invalid_existing + invalid_creates;

    let relevance_score = if planned_relevant_files > 0 { 1.0 } else { 0.0 };
    let validity_score = if invalid_planned_files == 0 { 1.0 } else { 0.0 };
    let unrelated_score = 1.0 / (1.0 + unrelated_planned_files as f32);
    let verification_score = if patch_plan.verification_commands.is_empty() {
        0.0
    } else {
        1.0
    };
    let risk_score = match patch_plan.risk_level {
        crate::pipeline::RiskLevel::Low => 1.0,
        crate::pipeline::RiskLevel::Medium => 0.8,
        crate::pipeline::RiskLevel::High => 0.3,
    };
    let score = 0.40 * relevance_score
        + 0.25 * validity_score
        + 0.20 * unrelated_score
        + 0.10 * verification_score
        + 0.05 * risk_score;

    PatchPlanQuality {
        invalid_planned_files,
        score,
    }
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
                invalid_planned_files: dry_metrics
                    .iter()
                    .map(|metrics| metrics.invalid_planned_files)
                    .sum(),
                avg_patch_plan_quality: if dry_run_count == 0 {
                    0.0
                } else {
                    dry_metrics
                        .iter()
                        .map(|metrics| metrics.patch_plan_quality_score)
                        .sum::<f32>()
                        / dry_run_count as f32
                },
                total_cost_usd: clean_zero(
                    dry_metrics
                        .iter()
                        .map(|metrics| metrics.total_cost_usd)
                        .sum(),
                ),
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

fn clean_zero(value: f32) -> f32 {
    if value.abs() < 0.000_001 { 0.0 } else { value }
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

/// Per-task per-mode metrics rolled up for warm-vs-off comparisons.
struct PairedMetric<'a> {
    off: &'a BenchTaskResult,
    other: &'a BenchTaskResult,
}

fn pair_mode_against_off<'a>(report: &'a BenchReport, other_mode: &str) -> Vec<PairedMetric<'a>> {
    let mut by_task: BTreeMap<&str, BTreeMap<&str, &BenchTaskResult>> = BTreeMap::new();
    for result in &report.results {
        by_task
            .entry(result.task_id.as_str())
            .or_default()
            .insert(result.mode.as_str(), result);
    }
    let mut out = Vec::new();
    for (_, modes) in by_task {
        if let (Some(off), Some(other)) = (modes.get("brain_off"), modes.get(other_mode)) {
            out.push(PairedMetric { off, other });
        }
    }
    out
}

fn render_falsifiable_claim_summary(report: &BenchReport) -> String {
    let warm_pairs = pair_mode_against_off(report, "brain_on_warm");
    let auto_pairs = pair_mode_against_off(report, "brain_on_auto_warm");
    let ablation_pairs = pair_mode_against_off(report, "brain_on_auto_warm_no_memory");
    if warm_pairs.is_empty() && auto_pairs.is_empty() && ablation_pairs.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("## Falsifiable Claim — warm vs off\n\n");
    out.push_str("docs/archive/MVP.md threshold: a warm mode beats off on **at least one** of\n");
    out.push_str("- ≥20% fewer total tool calls\n");
    out.push_str("- ≥15 pp success-rate uplift\n");
    out.push_str("- strictly fewer verification retries\n\n");
    out.push_str(
        "`brain_on_warm` is hand-curated. `brain_on_auto_warm` carries only memories the agent itself proposed and survived auto-accept. `brain_on_auto_warm_no_memory` is the MP-1.7 ablation: same flow as auto_warm but with the `memories` projection wiped between phases, so the followup sees only repo / prior_run capsules. The delta between auto_warm and auto_warm_no_memory isolates the actual contribution of accepted memories.\n\n",
    );

    if !warm_pairs.is_empty() {
        out.push_str(&render_paired_block("brain_on_warm", &warm_pairs));
    }
    if !auto_pairs.is_empty() {
        out.push_str(&render_paired_block("brain_on_auto_warm", &auto_pairs));
    }
    if !ablation_pairs.is_empty() {
        out.push_str(&render_paired_block(
            "brain_on_auto_warm_no_memory",
            &ablation_pairs,
        ));
    }
    out
}

fn render_paired_block(mode_label: &str, pairs: &[PairedMetric<'_>]) -> String {
    let mut other_successes = 0u32;
    let mut off_successes = 0u32;
    let mut other_tool_calls = 0u64;
    let mut off_tool_calls = 0u64;
    let mut other_retries = 0u64;
    let mut off_retries = 0u64;
    let mut paired_real_fix = 0u32;

    for PairedMetric { off, other } in pairs {
        if off.success {
            off_successes += 1;
        }
        if other.success {
            other_successes += 1;
        }
        if let (Some(off_m), Some(other_m)) = (off.dry_run.as_ref(), other.dry_run.as_ref()) {
            off_tool_calls += off_m.tool_calls as u64;
            other_tool_calls += other_m.tool_calls as u64;
            off_retries += off_m.verification_attempts.saturating_sub(1) as u64;
            other_retries += other_m.verification_attempts.saturating_sub(1) as u64;
            if off_m.verification_attempts > 0 || other_m.verification_attempts > 0 {
                paired_real_fix += 1;
            }
        }
    }

    let total = pairs.len() as u32;
    let success_rate_off = ratio(off_successes as usize, total as usize);
    let success_rate_other = ratio(other_successes as usize, total as usize);
    let success_uplift_pp = (success_rate_other - success_rate_off) * 100.0;
    let tool_calls_ratio = if off_tool_calls == 0 {
        f32::NAN
    } else {
        other_tool_calls as f32 / off_tool_calls as f32
    };
    let tool_calls_pct_drop = if tool_calls_ratio.is_nan() {
        f32::NAN
    } else {
        (1.0 - tool_calls_ratio) * 100.0
    };
    let retries_delta = off_retries as i64 - other_retries as i64;

    let tool_call_pass = !tool_calls_pct_drop.is_nan() && tool_calls_pct_drop >= 20.0;
    let success_pass = success_uplift_pp >= 15.0;
    let retries_pass = retries_delta > 0;
    let any_pass = tool_call_pass || success_pass || retries_pass;

    let mut out = String::new();
    out.push_str(&format!("### {mode_label} vs brain_off\n\n"));
    out.push_str(&format!(
        "Paired tasks: {total} (real-fix subset where Verification ran: {paired_real_fix}).\n\n"
    ));
    out.push_str(&format!(
        "| metric | brain_off | {mode_label} | delta | threshold | pass |\n\
         | --- | ---: | ---: | ---: | --- | :---: |\n"
    ));
    out.push_str(&format!(
        "| success rate | {:.0}% | {:.0}% | {:+.0} pp | ≥15 pp | {} |\n",
        success_rate_off * 100.0,
        success_rate_other * 100.0,
        success_uplift_pp,
        if success_pass { "✓" } else { "✗" },
    ));
    let tool_delta_str = if tool_calls_pct_drop.is_nan() {
        "n/a".to_string()
    } else {
        format!("{:+.0}%", -tool_calls_pct_drop)
    };
    out.push_str(&format!(
        "| total tool calls | {off_tool_calls} | {other_tool_calls} | {tool_delta_str} | -20% or better | {} |\n",
        if tool_call_pass { "✓" } else { "✗" },
    ));
    out.push_str(&format!(
        "| verification retries | {off_retries} | {other_retries} | {:+} | strictly fewer | {} |\n\n",
        -retries_delta,
        if retries_pass { "✓" } else { "✗" },
    ));
    out.push_str(&format!(
        "**{mode_label}: claim {}**: at least one threshold {}.\n\n",
        if any_pass { "passes" } else { "fails" },
        if any_pass { "cleared" } else { "NOT cleared" },
    ));
    out
}

fn render_report(report: &BenchReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Kimetsu Bench {}\n\n", report.bench_run_id));
    out.push_str(&format!("Tasks: {}\n\n", report.task_count));
    out.push_str(&format!(
        "Model-backed: {}. Cost cap: ${:.4}. Observed model cost: ${:.4}.\n\n",
        report.model_backed, report.max_cost_usd, report.total_cost_usd
    ));
    if let Some(reason) = &report.stopped_reason {
        out.push_str(&format!("Stopped early: {reason}.\n\n"));
    }
    if report.model_backed {
        out.push_str("This benchmark runs the full pipeline through the configured model provider for every mode. brain_off disables the broker (no memory or prior_run capsules); brain_on_cold uses the broker with no warm seed; brain_on_warm uses the broker plus a warm-seeded memory. Real-fix fixtures (dry_run=false) execute Implementation and Verification end-to-end with the failure-fingerprint retry loop; retrieval-only fixtures (dry_run=true) stop after PatchPlan and measure broker quality.\n\n");
    } else {
        out.push_str("This benchmark measures context and memory retrieval and runs deterministic dry-run PatchPlan traces with the model disabled. Implementation and Verification stages are skipped.\n\n");
    }
    out.push_str(&render_falsifiable_claim_summary(report));
    out.push_str("## Summary\n\n");
    out.push_str("| mode | success | relevant_signal | memories | context | irrelevant_context | dry_runs | avg_ms | cost_usd | plan_quality | invalid_planned | trace_events | model_turns | model_skips | planned_relevant | unrelated_planned |\n");
    out.push_str(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for summary in &report.summaries {
        out.push_str(&format!(
            "| {} | {:.0}% | {:.0}% | {} | {} | {} | {} | {:.2} | {:.4} | {:.2} | {} | {} | {} | {} | {} | {} |\n",
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
            summary.planned_relevant_files,
            summary.unrelated_planned_files,
        ));
    }

    out.push_str("\n## Tasks\n\n");
    out.push_str("| task | category | mode | success | memories | context | irrelevant | run | duration_ms | cost_usd | plan_quality | invalid_planned | trace_events | model_turns | planned_relevant | unrelated_planned |\n");
    out.push_str(
        "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for result in &report.results {
        let dry_run = result.dry_run.as_ref();
        let terminal_display = dry_run
            .map(
                |metrics| match (&metrics.terminal_kind, &metrics.failure_category) {
                    (k, Some(cat)) if k == "run.failed" => format!("run.failed({cat})"),
                    (k, _) => k.clone(),
                },
            )
            .unwrap_or_else(|| "not_run".to_string());
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {:.2} | {:.4} | {:.2} | {} | {} | {} | {} | {} |\n",
            result.task_id,
            result.category,
            result.mode,
            if result.success { "yes" } else { "no" },
            result.accepted_memories_used,
            result.context_loads,
            result.irrelevant_context_loaded,
            terminal_display,
            dry_run
                .map(|metrics| metrics.duration_us as f32 / 1_000.0)
                .unwrap_or(0.0),
            dry_run.map(|metrics| metrics.total_cost_usd).unwrap_or(0.0),
            dry_run
                .map(|metrics| metrics.patch_plan_quality_score)
                .unwrap_or(0.0),
            dry_run
                .map(|metrics| metrics.invalid_planned_files)
                .unwrap_or(0),
            dry_run.map(|metrics| metrics.trace_events).unwrap_or(0),
            dry_run.map(|metrics| metrics.model_turns).unwrap_or(0),
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
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
            dry_run: true,
        },
        BenchTask {
            id: "rust_area_bug",
            category: "rust",
            seed_task: "Add a `rectangle::area` helper.",
            followup_task: "Fix the failing tests in src/lib.rs so cargo test passes.",
            memory_kind: MemoryKind::FailurePattern,
            warm_memory: "rectangle::area in src/lib.rs is supposed to return width * height; the failing tests in tests::area_of_3_by_4_is_12 and tests::area_of_5_by_5_is_25 document the bug.",
            relevant_handles: vec!["file:src/lib.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"kimetsu-bench-area\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
                ),
                file(
                    "src/lib.rs",
                    "/// Returns the rectangle area for the given dimensions.\n///\n/// BUG: this implementation returns `width + height` instead of `width * height`.\n/// The failing tests in this module document the desired behavior.\npub fn area(width: u32, height: u32) -> u32 {\n    width + height\n}\n\n#[cfg(test)]\nmod tests {\n    use super::area;\n\n    #[test]\n    fn area_of_3_by_4_is_12() {\n        assert_eq!(area(3, 4), 12);\n    }\n\n    #[test]\n    fn area_of_5_by_5_is_25() {\n        assert_eq!(area(5, 5), 25);\n    }\n}\n",
                ),
            ],
            dry_run: false,
        },
        // Memory-dependent: failing test imports `crate::v2::compute`, only `v1`
        // exists. The fix can be either renaming v1 to v2 or adding a v2
        // re-export module. The warm memory says re-export through v2 to keep
        // v1 available; without it the agent has no way to know which choice
        // matches the project convention.
        BenchTask {
            id: "rust_module_namespacing",
            category: "rust",
            seed_task: "Set up versioned public API modules under v1 and v2.",
            followup_task: "Make cargo test pass without removing the existing v1 module.",
            memory_kind: MemoryKind::Convention,
            warm_memory: "Public API convention: v2 is the current namespace. Always re-export v1 helpers through src/v2/mod.rs as `pub use crate::v1::compute;`. Never delete v1; never edit tests to dodge the missing v2.",
            relevant_handles: vec!["file:src/v2/mod.rs", "file:src/lib.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"kimetsu-bench-namespacing\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
                ),
                file(
                    "src/lib.rs",
                    "//! Versioned public API for the kimetsu-bench-namespacing crate.\n//!\n//! Convention: v2 is the current namespace; v1 stays around for compatibility.\npub mod v1;\n",
                ),
                file(
                    "src/v1/mod.rs",
                    "/// Doubles its argument. v1 of the public compute helper.\npub fn compute(x: u32) -> u32 {\n    x * 2\n}\n",
                ),
                file(
                    "tests/api.rs",
                    "use kimetsu_bench_namespacing::v2::compute;\n\n#[test]\nfn v2_compute_is_re_exported_from_v1() {\n    assert_eq!(compute(21), 42);\n}\n",
                ),
            ],
            dry_run: false,
        },
        // Memory-dependent: the test calls `find_user`, the impl exposes
        // `get_user`. Two equally valid fixes (rename impl, or rename test).
        // The warm memory says rename `get_*` to `find_*` for fallible
        // lookups; without it the agent can't pick the convention.
        BenchTask {
            id: "rust_function_renamed",
            category: "rust",
            seed_task: "Add a fallible user lookup helper.",
            followup_task: "Make cargo test compile and pass without changing the test file.",
            memory_kind: MemoryKind::Convention,
            warm_memory: "Naming convention: fallible lookups are `find_*` (returning Option/Result), infallible lookups are `get_*`. Existing `get_user` in src/users.rs returns Option<User> and must be renamed to `find_user`. Tests are the source of truth for public names.",
            relevant_handles: vec!["file:src/users.rs", "file:src/lib.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"kimetsu-bench-rename\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
                ),
                file("src/lib.rs", "pub mod users;\n"),
                file(
                    "src/users.rs",
                    "#[derive(Debug, PartialEq, Eq)]\npub struct User {\n    pub id: u32,\n    pub name: String,\n}\n\npub fn get_user(id: u32) -> Option<User> {\n    if id == 1 {\n        Some(User { id, name: \"alice\".to_string() })\n    } else {\n        None\n    }\n}\n",
                ),
                file(
                    "tests/users.rs",
                    "use kimetsu_bench_rename::users::{find_user, User};\n\n#[test]\nfn find_user_returns_alice() {\n    assert_eq!(\n        find_user(1),\n        Some(User { id: 1, name: \"alice\".to_string() })\n    );\n}\n\n#[test]\nfn find_user_returns_none_for_unknown() {\n    assert_eq!(find_user(99), None);\n}\n",
                ),
            ],
            dry_run: false,
        },
        // Retry-stress: two correlated bugs across two files; only one is
        // surfaced per failing test. The agent that fixes only one will see
        // the other test fail on Verification and need to retry. The warm
        // memory hints both files are bugged.
        BenchTask {
            id: "rust_two_file_bug",
            category: "rust",
            seed_task: "Add area and perimeter helpers for rectangles.",
            followup_task: "Make cargo test pass.",
            memory_kind: MemoryKind::FailurePattern,
            warm_memory: "Both src/area.rs and src/perimeter.rs ship with off-by-operator bugs. area returns sum instead of product; perimeter returns product instead of `2 * (width + height)`. Fix both before re-running cargo test.",
            relevant_handles: vec!["file:src/area.rs", "file:src/perimeter.rs"],
            files: vec![
                file(
                    "Cargo.toml",
                    "[package]\nname = \"kimetsu-bench-two-file\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
                ),
                file("src/lib.rs", "pub mod area;\npub mod perimeter;\n"),
                file(
                    "src/area.rs",
                    "/// BUG: returns width + height instead of width * height.\npub fn area(width: u32, height: u32) -> u32 {\n    width + height\n}\n",
                ),
                file(
                    "src/perimeter.rs",
                    "/// BUG: returns width * height instead of 2 * (width + height).\npub fn perimeter(width: u32, height: u32) -> u32 {\n    width * height\n}\n",
                ),
                file(
                    "tests/geom.rs",
                    "use kimetsu_bench_two_file::{area::area, perimeter::perimeter};\n\n#[test]\nfn area_of_3_by_4_is_12() {\n    assert_eq!(area(3, 4), 12);\n}\n\n#[test]\nfn perimeter_of_3_by_4_is_14() {\n    assert_eq!(perimeter(3, 4), 14);\n}\n",
                ),
            ],
            dry_run: false,
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
        // Isolate from the developer's real user brain
        // (`~/.kimetsu/brain.db`): the multi-task run opens many
        // connections, and sharing the one real file serializes them
        // into SQLITE_BUSY. Fixture repos are already git-isolated (see
        // run_task_mode), so this only scopes the user brain.
        kimetsu_brain::user_brain::with_user_brain_disabled(|| {
            let repo = std::env::temp_dir().join(format!("kimetsu-bench-report-{}", new_id()));
            fs::create_dir_all(&repo).expect("create repo");

            let report = run_benchmark(BenchOptions {
                repo: repo.clone(),
                keep_fixtures: false,
                model_backed: false,
                limit: None,
                max_cost_usd: 1.0,
            })
            .expect("run benchmark");

            assert_eq!(report.task_count, 16);
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
        });
    }

    #[test]
    fn auto_accept_policy_matches_documented_thresholds() {
        let empty_pool: &[ProposalShadow] = &[];

        // preference / global_user accepts at >= 0.8
        let strong_pref = project::ProposalRow {
            proposal_id: "p1".into(),
            run_id: "r".into(),
            scope: "global_user".into(),
            kind: "preference".into(),
            text: "Prefer rg over grep.".into(),
            rationale: String::new(),
            proposed_confidence: 0.85,
            status: "pending".into(),
            decided_reason: None,
        };
        assert!(auto_accept_policy_allows(&strong_pref, empty_pool));

        let weak_pref = project::ProposalRow {
            proposed_confidence: 0.6,
            ..strong_pref.clone()
        };
        assert!(!auto_accept_policy_allows(&weak_pref, empty_pool));

        // preference at non-global_user scope: never auto-accept
        let scoped_pref = project::ProposalRow {
            scope: "repo".into(),
            ..strong_pref.clone()
        };
        assert!(!auto_accept_policy_allows(&scoped_pref, empty_pool));

        // convention at repo or project scope >= 0.7 accepts (MP-1.6: reverted
        // from 0.6 after the second bench showed mid-confidence conventions
        // regressing real-fix tasks. Relevance gate (MP-1.8) is the
        // structural fix; this threshold is the conservative interim).
        let repo_conv = project::ProposalRow {
            proposal_id: "p2".into(),
            run_id: "r".into(),
            scope: "repo".into(),
            kind: "convention".into(),
            text: "Use find_* for fallible lookups.".into(),
            rationale: String::new(),
            proposed_confidence: 0.7,
            status: "pending".into(),
            decided_reason: None,
        };
        assert!(auto_accept_policy_allows(&repo_conv, empty_pool));

        let project_conv = project::ProposalRow {
            scope: "project".into(),
            ..repo_conv.clone()
        };
        assert!(auto_accept_policy_allows(&project_conv, empty_pool));

        let weak_conv = project::ProposalRow {
            proposed_confidence: 0.69,
            ..repo_conv.clone()
        };
        assert!(!auto_accept_policy_allows(&weak_conv, empty_pool));

        // failure_pattern is always rejected for auto-accept
        let failure = project::ProposalRow {
            proposal_id: "p3".into(),
            run_id: "r".into(),
            scope: "repo".into(),
            kind: "failure_pattern".into(),
            text: "Same fingerprint twice means restart implementation.".into(),
            rationale: String::new(),
            proposed_confidence: 0.99,
            status: "pending".into(),
            decided_reason: None,
        };
        assert!(!auto_accept_policy_allows(&failure, empty_pool));

        // Empty text is never accepted regardless of category.
        let empty = project::ProposalRow {
            text: "   ".into(),
            ..strong_pref
        };
        assert!(!auto_accept_policy_allows(&empty, empty_pool));
    }

    /// MP-4c: a proposal that re-states an existing memory with
    /// use_count >= 3 and usefulness ratio < -0.2 (same scope+kind, normalized
    /// token Jaccard >= 0.5) must be rejected even when the confidence would
    /// otherwise pass. Lets the brain stop re-injecting the same hurting
    /// convention every seed run.
    #[test]
    fn auto_accept_shadowed_by_low_usefulness_memory_is_rejected() {
        let proposal = project::ProposalRow {
            proposal_id: "p_shadow".into(),
            run_id: "r".into(),
            scope: "repo".into(),
            kind: "convention".into(),
            text: "Use find_* for fallible lookups.".into(),
            rationale: String::new(),
            proposed_confidence: 0.9,
            status: "pending".into(),
            decided_reason: None,
        };

        // Memory that has hurt 4 of 4 runs (usefulness ratio = -1.0).
        let bad_shadow = ProposalShadow {
            scope: "repo".into(),
            kind: "convention".into(),
            normalized_text: kimetsu_core::memory::normalize_memory_text(
                "Use find_ prefix for fallible lookups.",
            ),
            use_count: 4,
            usefulness_score: -4.0,
        };
        assert!(!auto_accept_policy_allows(
            &proposal,
            std::slice::from_ref(&bad_shadow)
        ));

        // Small sample (use_count < 3) does NOT shadow even when ratio is bad.
        let small_sample = ProposalShadow {
            use_count: 2,
            usefulness_score: -2.0,
            ..bad_shadow.clone()
        };
        assert!(auto_accept_policy_allows(&proposal, &[small_sample]));

        // Ratio above -0.2 does NOT shadow even with enough samples.
        let neutral_shadow = ProposalShadow {
            use_count: 10,
            usefulness_score: 0.0,
            ..bad_shadow.clone()
        };
        assert!(auto_accept_policy_allows(&proposal, &[neutral_shadow]));

        // Different scope does not shadow.
        let wrong_scope = ProposalShadow {
            scope: "global_user".into(),
            ..bad_shadow.clone()
        };
        assert!(auto_accept_policy_allows(&proposal, &[wrong_scope]));

        // Different kind does not shadow.
        let wrong_kind = ProposalShadow {
            kind: "preference".into(),
            ..bad_shadow.clone()
        };
        assert!(auto_accept_policy_allows(&proposal, &[wrong_kind]));

        // Low overlap (<0.5 Jaccard) does not shadow.
        let unrelated = ProposalShadow {
            normalized_text: kimetsu_core::memory::normalize_memory_text(
                "Always run cargo fmt before committing.",
            ),
            ..bad_shadow
        };
        assert!(auto_accept_policy_allows(&proposal, &[unrelated]));
    }
}
