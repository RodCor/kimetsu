use kimetsu_core::memory::MemoryKind;
use serde::{Deserialize, Serialize};

use crate::context::{ContextBundle, ContextCapsule};

pub const DEFAULT_BENCHMARK_DATASET: &str = "terminal-bench/terminal-bench-2";

const TERMINAL_BENCH_SLUGS: &[&str] = &[
    "make-mips-interpreter",
    "circuit-fibsqrt",
    "build-pov-ray",
    "overfull-hbox",
    "distribution-search",
    "break-filter-js-from-html",
    "video-processing",
    "protein-assembly",
    "path-tracing",
    "compile-compcert",
    "log-summary-date-ranges",
    "openssl-selfsigned-cert",
    "dna-assembly",
    "caffe-cifar-10",
    "install-windows-3-11",
    "vulnerable-secret",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkWarmPolicy {
    ColdBrain,
    ReactiveWarm,
    FullWarm,
}

impl BenchmarkWarmPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "full" | "full_warm" | "warm" | "brain_on_warm" => Some(Self::FullWarm),
            "reactive" | "reactive_warm" | "warm_reactive" | "optional_warm" => {
                Some(Self::ReactiveWarm)
            }
            "cold" | "cold_brain" | "brain_on_cold" => Some(Self::ColdBrain),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ColdBrain => "cold_brain",
            Self::ReactiveWarm => "reactive_warm",
            Self::FullWarm => "full_warm",
        }
    }

    pub const fn playbook_note(self) -> &'static str {
        match self {
            Self::ColdBrain => {
                "Cold brain: memory capsules are intentionally excluded. This measures broker/repo/prior-run grounding without accepted memories."
            }
            Self::ReactiveWarm => {
                "Reactive warm: Kimetsu memory is available when the harness or model asks for it, but task-specific benchmark memory is not required."
            }
            Self::FullWarm => {
                "Full warm: the benchmark playbook is fetched before the task starts and may include task-specific benchmark memories."
            }
        }
    }
}

impl Default for BenchmarkWarmPolicy {
    fn default() -> Self {
        Self::FullWarm
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkMemoryRole {
    /// Exact run/task outcome memory. Useful evidence, but low-priority
    /// guidance because it often overfits one benchmark instance.
    Episodic,
    /// A reusable tactic or operator that can transfer across task slugs.
    SemanticOperator,
    /// A reusable warning about a failure mode or misleading tactic.
    AntiPattern,
}

impl BenchmarkMemoryRole {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "episodic" | "run" | "task_run" | "outcome" => Some(Self::Episodic),
            "semantic" | "semantic_operator" | "operator" | "tactic" | "recipe" => {
                Some(Self::SemanticOperator)
            }
            "anti" | "anti_pattern" | "antipattern" | "failure_pattern" | "warning" => {
                Some(Self::AntiPattern)
            }
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Episodic => "episodic",
            Self::SemanticOperator => "semantic_operator",
            Self::AntiPattern => "anti_pattern",
        }
    }

    pub const fn is_generalizable(self) -> bool {
        matches!(self, Self::SemanticOperator | Self::AntiPattern)
    }
}

impl Default for BenchmarkMemoryRole {
    fn default() -> Self {
        Self::Episodic
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkBrainContext {
    pub dataset: String,
    pub task: String,
    pub task_slug: Option<String>,
    pub warm_policy: BenchmarkWarmPolicy,
    pub query: String,
    pub stage: String,
    pub budget_tokens: u32,
    pub used_tokens: u32,
    pub capsule_count: usize,
    pub memory_capsule_count: usize,
    pub benchmark_memory_count: usize,
    pub generalizable_memory_count: usize,
    pub episodic_memory_count: usize,
    pub required_ok: bool,
    pub playbook_markdown: String,
    pub capsules: Vec<ContextCapsule>,
    pub excluded: Vec<ContextCapsule>,
}

#[derive(Debug, Clone)]
pub struct BenchmarkMemoryProposal {
    pub role: BenchmarkMemoryRole,
    pub text: String,
    pub task_family: Option<String>,
    pub applies_to: Vec<String>,
    pub does_not_apply_to: Vec<String>,
    pub evidence_for: Vec<String>,
    pub evidence_against: Vec<String>,
    pub rationale: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Default)]
pub struct BenchmarkOutcome {
    pub task: String,
    pub dataset: String,
    pub task_slug: Option<String>,
    pub warm_policy: BenchmarkWarmPolicy,
    pub mode: String,
    pub passed: Option<bool>,
    pub score: Option<f32>,
    pub error: Option<String>,
    pub summary: String,
    pub commands: Vec<String>,
    pub pitfalls: Vec<String>,
    pub verify: Vec<String>,
    pub cost_usd: Option<f32>,
    pub duration_seconds: Option<f32>,
    pub generalization: Option<BenchmarkMemoryProposal>,
}

pub fn normalize_task_slug(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    for slug in TERMINAL_BENCH_SLUGS {
        if lower.contains(slug) {
            return Some((*slug).to_string());
        }
    }

    lower
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
        .filter_map(|token| {
            let token = token.split("__").next().unwrap_or(token);
            let token = token.trim_matches('-').replace('_', "-");
            if looks_like_slug(&token) {
                Some(token)
            } else {
                None
            }
        })
        .next()
}

pub fn benchmark_query(
    task: &str,
    dataset: &str,
    task_slug: Option<&str>,
    warm_policy: BenchmarkWarmPolicy,
) -> String {
    let compact_task = compact_text(task, 1400);
    match task_slug {
        Some(slug) => format!(
            "terminal-bench benchmark dataset:{dataset} warm-policy:{} terminal-bench:{slug} benchmark:{slug} task-slug:{slug} slug-words:{} task: {compact_task}",
            warm_policy.as_str(),
            slug.replace('-', " ")
        ),
        None => format!(
            "terminal-bench benchmark dataset:{dataset} warm-policy:{} task: {compact_task}",
            warm_policy.as_str()
        ),
    }
}

pub fn build_benchmark_context(
    bundle: ContextBundle,
    task: &str,
    dataset: &str,
    query: &str,
    task_slug: Option<String>,
    warm_policy: BenchmarkWarmPolicy,
    require_benchmark_memory: bool,
    max_capsules: usize,
) -> BenchmarkBrainContext {
    let max_capsules = max_capsules.clamp(1, 20);
    let selected = prioritized_capsules(
        &bundle.capsules,
        task_slug.as_deref(),
        warm_policy,
        max_capsules,
    );
    let memory_capsule_count = selected
        .iter()
        .filter(|capsule| capsule.kind == "memory")
        .count();
    let benchmark_memory_count = selected
        .iter()
        .filter(|capsule| benchmark_memory_matches(capsule, task_slug.as_deref()))
        .count();
    let generalizable_memory_count = selected
        .iter()
        .filter(|capsule| {
            benchmark_memory_role(capsule).is_some_and(BenchmarkMemoryRole::is_generalizable)
        })
        .count();
    let episodic_memory_count = selected
        .iter()
        .filter(|capsule| benchmark_memory_role(capsule) == Some(BenchmarkMemoryRole::Episodic))
        .count();
    let required_ok =
        !require_benchmark_memory || benchmark_memory_count > 0 || generalizable_memory_count > 0;
    let playbook_markdown = format_playbook(
        dataset,
        task,
        task_slug.as_deref(),
        warm_policy,
        query,
        required_ok,
        memory_capsule_count,
        benchmark_memory_count,
        generalizable_memory_count,
        episodic_memory_count,
        &selected,
    );

    BenchmarkBrainContext {
        dataset: dataset.to_string(),
        task: task.to_string(),
        task_slug,
        warm_policy,
        query: query.to_string(),
        stage: bundle.stage,
        budget_tokens: bundle.budget_tokens,
        used_tokens: bundle.used_tokens,
        capsule_count: selected.len(),
        memory_capsule_count,
        benchmark_memory_count,
        generalizable_memory_count,
        episodic_memory_count,
        required_ok,
        playbook_markdown,
        capsules: selected,
        excluded: bundle.excluded,
    }
}

pub fn benchmark_memory_matches(capsule: &ContextCapsule, task_slug: Option<&str>) -> bool {
    if capsule.kind != "memory" {
        return false;
    }
    let Some(slug) = task_slug else {
        return false;
    };
    let slug = slug.to_ascii_lowercase();
    let haystack = capsule_text(capsule);
    haystack.contains(&format!("terminal-bench:{slug}"))
        || haystack.contains(&format!("benchmark:{slug}"))
        || haystack.contains(&format!("task-slug:{slug}"))
        || haystack.contains(&slug)
}

pub fn benchmark_memory_role(capsule: &ContextCapsule) -> Option<BenchmarkMemoryRole> {
    if capsule.kind != "memory" {
        return None;
    }
    let haystack = capsule_text(capsule);
    if has_role_marker(&haystack, BenchmarkMemoryRole::SemanticOperator) {
        return Some(BenchmarkMemoryRole::SemanticOperator);
    }
    if has_role_marker(&haystack, BenchmarkMemoryRole::AntiPattern) {
        return Some(BenchmarkMemoryRole::AntiPattern);
    }
    if has_role_marker(&haystack, BenchmarkMemoryRole::Episodic) {
        return Some(BenchmarkMemoryRole::Episodic);
    }
    if haystack.contains("[terminal-bench:") && haystack.contains("status=") {
        return Some(BenchmarkMemoryRole::Episodic);
    }
    None
}

fn has_role_marker(haystack: &str, role: BenchmarkMemoryRole) -> bool {
    let role = role.as_str();
    haystack.contains(&format!("memory_role={role}"))
        || haystack.contains(&format!("memory-role={role}"))
        || haystack.contains(&format!("role={role}"))
}

pub fn outcome_memory_kind(outcome: &BenchmarkOutcome) -> MemoryKind {
    if outcome
        .error
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return MemoryKind::FailurePattern;
    }
    match outcome.passed {
        Some(false) => MemoryKind::FailurePattern,
        Some(true) => MemoryKind::Command,
        None => MemoryKind::Fact,
    }
}

pub fn outcome_memory_text(outcome: &BenchmarkOutcome) -> String {
    let task_slug = outcome
        .task_slug
        .clone()
        .or_else(|| normalize_task_slug(&outcome.task))
        .unwrap_or_else(|| "unknown".to_string());
    let status = match (outcome.passed, outcome.error.as_deref()) {
        (_, Some(error)) if !error.trim().is_empty() => "error",
        (Some(true), _) => "pass",
        (Some(false), _) => "fail",
        (None, _) => "observed",
    };

    let mut parts = vec![format!(
        "[terminal-bench:{task_slug}] dataset={} mode={} status={status}",
        compact_text(&outcome.dataset, 120),
        compact_text(&outcome.mode, 80),
    )];
    parts.push(format!(
        "memory_role={}",
        BenchmarkMemoryRole::Episodic.as_str()
    ));
    parts.push(format!("warm_policy={}", outcome.warm_policy.as_str()));
    if let Some(score) = outcome.score {
        parts.push(format!("score={score:.3}"));
    }
    if let Some(cost_usd) = outcome.cost_usd {
        parts.push(format!("cost_usd={cost_usd:.4}"));
    }
    if let Some(duration) = outcome.duration_seconds {
        parts.push(format!("duration_seconds={duration:.1}"));
    }
    if !outcome.summary.trim().is_empty() {
        parts.push(format!("Summary: {}", compact_text(&outcome.summary, 500)));
    }
    if !outcome.commands.is_empty() {
        parts.push(format!(
            "Commands: {}",
            compact_text(&outcome.commands.join("; "), 350)
        ));
    }
    if !outcome.pitfalls.is_empty() {
        parts.push(format!(
            "Pitfalls: {}",
            compact_text(&outcome.pitfalls.join("; "), 350)
        ));
    }
    if !outcome.verify.is_empty() {
        parts.push(format!(
            "Verify: {}",
            compact_text(&outcome.verify.join("; "), 250)
        ));
    }
    if let Some(error) = outcome
        .error
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!("Error: {}", compact_text(error, 250)));
    }
    parts.join(". ")
}

pub fn proposal_memory_kind(proposal: &BenchmarkMemoryProposal) -> MemoryKind {
    match proposal.role {
        BenchmarkMemoryRole::AntiPattern => MemoryKind::FailurePattern,
        BenchmarkMemoryRole::SemanticOperator | BenchmarkMemoryRole::Episodic => {
            MemoryKind::Command
        }
    }
}

pub fn proposal_memory_text(
    outcome: &BenchmarkOutcome,
    proposal: &BenchmarkMemoryProposal,
) -> String {
    let task_slug = outcome
        .task_slug
        .clone()
        .or_else(|| normalize_task_slug(&outcome.task))
        .unwrap_or_else(|| "unknown".to_string());
    let mut parts = vec![format!(
        "[terminal-bench-memory] memory_role={} source_task_slug={} dataset={} mode={}",
        proposal.role.as_str(),
        task_slug,
        compact_text(&outcome.dataset, 120),
        compact_text(&outcome.mode, 80),
    )];
    if let Some(task_family) = proposal
        .task_family
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("task_family={}", compact_text(task_family, 120)));
    }
    parts.push(format!("Rule: {}", compact_text(&proposal.text, 700)));
    if !proposal.applies_to.is_empty() {
        parts.push(format!(
            "Applies_to: {}",
            compact_text(&proposal.applies_to.join("; "), 350)
        ));
    }
    if !proposal.does_not_apply_to.is_empty() {
        parts.push(format!(
            "Does_not_apply_to: {}",
            compact_text(&proposal.does_not_apply_to.join("; "), 350)
        ));
    }
    let evidence_for = if proposal.evidence_for.is_empty() {
        vec![task_slug]
    } else {
        proposal.evidence_for.clone()
    };
    parts.push(format!(
        "Evidence_for: {}",
        compact_text(&evidence_for.join("; "), 250)
    ));
    if !proposal.evidence_against.is_empty() {
        parts.push(format!(
            "Evidence_against: {}",
            compact_text(&proposal.evidence_against.join("; "), 250)
        ));
    }
    if !proposal.rationale.trim().is_empty() {
        parts.push(format!(
            "Review_rationale: {}",
            compact_text(&proposal.rationale, 250)
        ));
    }
    parts.push(
        "Human_review: pending; accept only if this transfers beyond the source task.".to_string(),
    );
    parts.join(". ")
}

fn prioritized_capsules(
    capsules: &[ContextCapsule],
    task_slug: Option<&str>,
    warm_policy: BenchmarkWarmPolicy,
    max_capsules: usize,
) -> Vec<ContextCapsule> {
    let mut ranked = capsules
        .iter()
        .enumerate()
        .map(|(idx, capsule)| {
            let role = benchmark_memory_role(capsule);
            let exact_task = benchmark_memory_matches(capsule, task_slug);
            let priority =
                if warm_policy == BenchmarkWarmPolicy::ColdBrain && capsule.kind == "memory" {
                    9
                } else if role.is_some_and(BenchmarkMemoryRole::is_generalizable) && exact_task {
                    0
                } else if role.is_some_and(BenchmarkMemoryRole::is_generalizable) {
                    1
                } else if exact_task {
                    2
                } else if capsule.kind == "memory" && role == Some(BenchmarkMemoryRole::Episodic) {
                    6
                } else if capsule.kind == "memory" {
                    3
                } else {
                    4
                };
            (priority, idx, capsule)
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| {
                right
                    .2
                    .score
                    .partial_cmp(&left.2.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    ranked
        .into_iter()
        .filter(|(_, _, capsule)| {
            warm_policy != BenchmarkWarmPolicy::ColdBrain || capsule.kind != "memory"
        })
        .take(max_capsules)
        .map(|(_, _, capsule)| capsule.clone())
        .collect()
}

fn format_playbook(
    dataset: &str,
    task: &str,
    task_slug: Option<&str>,
    warm_policy: BenchmarkWarmPolicy,
    query: &str,
    required_ok: bool,
    memory_capsule_count: usize,
    benchmark_memory_count: usize,
    generalizable_memory_count: usize,
    episodic_memory_count: usize,
    capsules: &[ContextCapsule],
) -> String {
    let mut out = String::new();
    out.push_str("# Kimetsu Benchmark Playbook\n\n");
    out.push_str(&format!("dataset: {dataset}\n"));
    out.push_str(&format!(
        "task_slug: {}\n",
        task_slug.unwrap_or("<not-detected>")
    ));
    out.push_str(&format!("warm_policy: {}\n", warm_policy.as_str()));
    out.push_str(&format!("required_ok: {required_ok}\n"));
    out.push_str(&format!("memory_capsule_count: {memory_capsule_count}\n"));
    out.push_str(&format!(
        "benchmark_memory_count: {benchmark_memory_count}\n"
    ));
    out.push_str(&format!(
        "generalizable_memory_count: {generalizable_memory_count}\n"
    ));
    out.push_str(&format!("episodic_memory_count: {episodic_memory_count}\n"));
    out.push('\n');
    out.push_str(warm_policy.playbook_note());
    out.push_str("\nUse these capsules as execution constraints before broad exploration. Prefer accepted semantic_operator and anti_pattern memories first because they are intended to transfer across tasks. Use exact episodic run summaries as evidence, not as dominant instructions.\n\n");

    if capsules.is_empty() {
        out.push_str("No capsules were retrieved. If this is required mode, inspect Kimetsu brain status and seed or ingest memory before benchmarking.\n\n");
    } else {
        for (idx, capsule) in capsules.iter().enumerate() {
            let role_text = benchmark_memory_role(capsule)
                .map(|role| format!(" role={}", role.as_str()))
                .unwrap_or_default();
            out.push_str(&format!(
                "{}. [{}{} score={:.3}] {}\n",
                idx + 1,
                capsule.kind,
                role_text,
                capsule.score,
                compact_text(&capsule.summary, 500)
            ));
            if !capsule.expansion_handle.trim().is_empty() {
                out.push_str(&format!("   source: {}\n", capsule.expansion_handle));
            }
        }
        out.push('\n');
    }

    out.push_str("# Retrieval Query\n\n");
    out.push_str(query);
    out.push_str("\n\n# Original Task\n\n");
    out.push_str(&compact_text(task, 1800));
    out
}

fn capsule_text(capsule: &ContextCapsule) -> String {
    let mut text = format!("{} {}", capsule.summary, capsule.expansion_handle);
    for provenance in &capsule.provenance {
        text.push(' ');
        text.push_str(&provenance.source);
        text.push(' ');
        text.push_str(&provenance.id);
        if let Some(excerpt) = &provenance.excerpt {
            text.push(' ');
            text.push_str(excerpt);
        }
    }
    text.to_ascii_lowercase()
}

fn looks_like_slug(token: &str) -> bool {
    token.len() >= 6
        && token.contains('-')
        && !is_generic_fallback_slug(token)
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && token.bytes().any(|byte| byte.is_ascii_alphabetic())
}

fn is_generic_fallback_slug(token: &str) -> bool {
    matches!(
        token,
        "terminal-bench"
            | "terminal-bench-2"
            | "kimetsu-mcp"
            | "kimetsu-brain"
            | "codex-kimetsu"
            | "full-warm"
            | "reactive-warm"
            | "cold-brain"
            | "warm-policy"
            | "task-slug"
            | "brain-context"
            | "benchmark-context"
    )
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= max_chars {
        return compact;
    }
    let mut truncated = compact.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextBundle, ContextCapsule, ProvenanceRef};

    #[test]
    fn detects_known_and_suffix_task_slugs() {
        assert_eq!(
            normalize_task_slug("compile-compcert__T6g5YAZ"),
            Some("compile-compcert".to_string())
        );
        assert_eq!(
            normalize_task_slug("Solve the build-pov-ray terminal task"),
            Some("build-pov-ray".to_string())
        );
    }

    #[test]
    fn ignores_generic_terminal_bench_tokens() {
        assert_eq!(
            normalize_task_slug("terminal-bench task: solve the benchmark"),
            None
        );
        assert_eq!(
            normalize_task_slug("dataset terminal-bench/terminal-bench-2 warm-policy full-warm"),
            None
        );
    }

    #[test]
    fn playbook_prioritizes_task_memory() {
        let memory = capsule(
            "memory",
            "[terminal-bench:compile-compcert] memory_role=episodic Redirect make logs and patch config.",
            "memory:1",
            0.7,
        );
        let repo = capsule("repo_file", "src/main.rs", "file:src/main.rs", 0.99);
        let bundle = ContextBundle {
            stage: "localization".to_string(),
            budget_tokens: 4000,
            used_tokens: 20,
            capsules: vec![repo, memory],
            excluded: Vec::new(),
        };

        let context = build_benchmark_context(
            bundle,
            "compile-compcert",
            DEFAULT_BENCHMARK_DATASET,
            "terminal-bench:compile-compcert",
            Some("compile-compcert".to_string()),
            BenchmarkWarmPolicy::FullWarm,
            true,
            8,
        );

        assert!(context.required_ok);
        assert_eq!(context.benchmark_memory_count, 1);
        assert!(context.capsules[0].summary.contains("compile-compcert"));
        assert!(
            context
                .playbook_markdown
                .contains("Kimetsu Benchmark Playbook")
        );
    }

    #[test]
    fn outcome_memory_text_marks_episodic() {
        let outcome = BenchmarkOutcome {
            task: "compile-compcert".to_string(),
            dataset: DEFAULT_BENCHMARK_DATASET.to_string(),
            mode: "required-kimetsu".to_string(),
            passed: Some(true),
            summary: "Configured tools and verified the build.".to_string(),
            ..BenchmarkOutcome::default()
        };

        let text = outcome_memory_text(&outcome);

        assert!(text.contains("[terminal-bench:compile-compcert]"));
        assert!(text.contains("memory_role=episodic"));
        assert!(text.contains("status=pass"));
    }

    #[test]
    fn proposal_memory_text_marks_generalizable_and_review_pending() {
        let outcome = BenchmarkOutcome {
            task: "compile-compcert".to_string(),
            dataset: DEFAULT_BENCHMARK_DATASET.to_string(),
            mode: "required-kimetsu".to_string(),
            ..BenchmarkOutcome::default()
        };
        let proposal = BenchmarkMemoryProposal {
            role: BenchmarkMemoryRole::SemanticOperator,
            text: "For generated-artifact tasks with hidden verifiers, build a small checker and validate randomized cases before finalizing.".to_string(),
            task_family: Some("generated-artifact-verification".to_string()),
            applies_to: vec!["tasks with hidden validators".to_string()],
            does_not_apply_to: vec!["pure installation tasks".to_string()],
            evidence_for: vec!["compile-compcert".to_string()],
            evidence_against: Vec::new(),
            rationale: "The lesson transfers beyond the exact task slug.".to_string(),
            confidence: 0.82,
        };

        let text = proposal_memory_text(&outcome, &proposal);

        assert!(text.contains("[terminal-bench-memory]"));
        assert!(text.contains("memory_role=semantic_operator"));
        assert!(text.contains("Human_review: pending"));
        assert!(text.contains("task_family=generated-artifact-verification"));
    }

    #[test]
    fn playbook_prioritizes_generalizable_memory_over_exact_episodic() {
        let repo = capsule("repo_file", "src/main.rs", "file:src/main.rs", 0.99);
        let semantic = capsule(
            "memory",
            "[terminal-bench-memory] memory_role=semantic_operator task_family=generated-artifact Rule: Build a local checker before finalizing.",
            "memory:semantic",
            0.5,
        );
        let episodic = capsule(
            "memory",
            "[terminal-bench:compile-compcert] memory_role=episodic status=pass Commands: ./configure; make -j2.",
            "memory:episodic",
            0.9,
        );
        let bundle = ContextBundle {
            stage: "localization".to_string(),
            budget_tokens: 4000,
            used_tokens: 20,
            capsules: vec![repo, episodic, semantic],
            excluded: Vec::new(),
        };

        let context = build_benchmark_context(
            bundle,
            "compile-compcert",
            DEFAULT_BENCHMARK_DATASET,
            "terminal-bench:compile-compcert",
            Some("compile-compcert".to_string()),
            BenchmarkWarmPolicy::FullWarm,
            true,
            8,
        );

        assert_eq!(context.generalizable_memory_count, 1);
        assert_eq!(context.episodic_memory_count, 1);
        assert_eq!(context.benchmark_memory_count, 1);
        assert!(context.capsules[0].summary.contains("semantic_operator"));
        assert!(context.playbook_markdown.contains("role=semantic_operator"));
        assert!(context.playbook_markdown.contains("role=episodic"));
    }

    #[test]
    fn required_mode_accepts_generalizable_memory_without_exact_slug() {
        let semantic = capsule(
            "memory",
            "[terminal-bench-memory] memory_role=anti_pattern task_family=generated-artifact Rule: Do not treat compile success as proof; run a behavioral verifier.",
            "memory:semantic",
            0.8,
        );
        let bundle = ContextBundle {
            stage: "localization".to_string(),
            budget_tokens: 4000,
            used_tokens: 20,
            capsules: vec![semantic],
            excluded: Vec::new(),
        };

        let context = build_benchmark_context(
            bundle,
            "compile-compcert",
            DEFAULT_BENCHMARK_DATASET,
            "terminal-bench:compile-compcert",
            Some("compile-compcert".to_string()),
            BenchmarkWarmPolicy::FullWarm,
            true,
            8,
        );

        assert!(context.required_ok);
        assert_eq!(context.benchmark_memory_count, 0);
        assert_eq!(context.generalizable_memory_count, 1);
    }

    #[test]
    fn cold_brain_excludes_memory_capsules() {
        let memory = capsule(
            "memory",
            "[terminal-bench:compile-compcert] Warm memory.",
            "memory:1",
            1.0,
        );
        let repo = capsule("repo_file", "src/main.rs", "file:src/main.rs", 0.5);
        let bundle = ContextBundle {
            stage: "localization".to_string(),
            budget_tokens: 4000,
            used_tokens: 20,
            capsules: vec![memory, repo],
            excluded: Vec::new(),
        };

        let context = build_benchmark_context(
            bundle,
            "compile-compcert",
            DEFAULT_BENCHMARK_DATASET,
            "terminal-bench:compile-compcert",
            Some("compile-compcert".to_string()),
            BenchmarkWarmPolicy::ColdBrain,
            false,
            8,
        );

        assert_eq!(context.memory_capsule_count, 0);
        assert_eq!(context.benchmark_memory_count, 0);
        assert!(
            context
                .capsules
                .iter()
                .all(|capsule| capsule.kind != "memory")
        );
        assert!(context.playbook_markdown.contains("cold_brain"));
    }

    fn capsule(kind: &str, summary: &str, handle: &str, score: f32) -> ContextCapsule {
        ContextCapsule {
            id: summary.to_string(),
            kind: kind.to_string(),
            summary: summary.to_string(),
            token_estimate: 10,
            expansion_handle: handle.to_string(),
            provenance: vec![ProvenanceRef {
                source: "test".to_string(),
                id: handle.to_string(),
                excerpt: Some(summary.to_string()),
            }],
            confidence: 1.0,
            freshness: 1.0,
            relevance: 1.0,
            scope_weight: 1.0,
            score,
        }
    }
}
