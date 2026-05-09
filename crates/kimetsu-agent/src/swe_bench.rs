//! SWE-bench task ingestion (scaffolding only in v0.1).
//!
//! This module reads SWE-bench-format task records from a JSONL file and runs
//! one or more of them through the existing Kimetsu coding pipeline. The full
//! integration (cloning the upstream repo at the recorded commit, applying the
//! `test_patch`, scoring against the gold patch) is documented in
//! `SWEBENCH.md` and tracked as v0.2 work; v0.1 ships only the task reader and
//! a CLI entry point that lets callers smoke-test individual instances against
//! a repo they have already prepared on disk.
//!
//! The intentional scope:
//!
//! - parse SWE-bench Lite / SWE-bench Pro instance JSONL into a typed shape
//!   without taking on a YAML or git-clone dependency
//! - drive `pipeline::run_coding` against a caller-supplied `repo` path
//! - emit a result payload that matches the schema documented in `SWEBENCH.md`
//!
//! Out of scope for this file (and v0.1):
//!
//! - cloning the upstream repo at `base_commit`
//! - applying `test_patch` programmatically before invoking the pipeline
//! - scoring the resulting patch against `patch` (the gold patch)
//! - distributed execution

use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_core::KimetsuResult;
use serde::{Deserialize, Serialize};

use crate::pipeline::{CodingRunOptions, CodingRunResult, run_coding};

/// One SWE-bench-style task record.
///
/// Field names mirror the public SWE-bench JSONL schema so downstream tooling
/// (and harness comparators like swebench-eval) can reuse the same identifiers.
/// Optional fields default to `None` when missing so SWE-bench Lite / Pro
/// variants can be ingested by the same parser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweBenchTask {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    pub problem_statement: String,
    #[serde(default)]
    pub hints_text: Option<String>,
    #[serde(default)]
    pub test_patch: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub fail_to_pass: Option<Vec<String>>,
    #[serde(default)]
    pub pass_to_pass: Option<Vec<String>>,
}

/// Options for `kimetsu bench swe`.
#[derive(Debug, Clone)]
pub struct SweBenchOptions {
    /// JSONL file holding one or more SWE-bench task records (one per line).
    pub tasks: PathBuf,
    /// Working repo to drive Kimetsu against. The caller is responsible for
    /// preparing this repo (correct commit, test_patch applied) — v0.1 does
    /// not clone or apply patches itself.
    pub repo: PathBuf,
    /// Optional instance_id filter; when set only that one task runs.
    pub instance_id: Option<String>,
    /// When true, runs `pipeline::run_coding` with `dry_run=true` (skips
    /// Implementation+Verification). Useful for checking PatchPlan quality on
    /// a SWE-bench instance without burning tokens or modifying the repo.
    pub dry_run: bool,
    /// When true, disables the broker so the pipeline runs without Kimetsu's
    /// memory/repo capsules. Lets you measure brain-on vs brain-off on a real
    /// SWE-bench instance.
    pub disable_broker: bool,
    /// Hard cap on tasks executed in one invocation.
    pub limit: Option<usize>,
}

/// Result of running one SWE-bench task through the pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct SweBenchInstanceResult {
    pub instance_id: String,
    pub repo: String,
    pub run_id: String,
    pub trace_path: PathBuf,
    pub final_report_path: PathBuf,
    pub patch_plan_id: String,
    pub dry_run: bool,
    pub disable_broker: bool,
}

/// Parse a JSONL file of SWE-bench tasks. Empty lines are skipped; malformed
/// trailing lines are tolerated, matching the trace.jsonl replay rules.
pub fn read_tasks(path: &Path) -> KimetsuResult<Vec<SweBenchTask>> {
    let contents = fs::read_to_string(path)?;
    let mut tasks = Vec::new();
    let mut iter = contents.lines().peekable();
    while let Some(raw) = iter.next() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<SweBenchTask>(line) {
            Ok(task) => tasks.push(task),
            Err(err) => {
                if iter.peek().is_none() {
                    eprintln!(
                        "kimetsu::swe_bench: ignoring malformed trailing line in {}: {err}",
                        path.display()
                    );
                    break;
                }
                return Err(format!(
                    "swe_bench task file {} contains a malformed line: {err}",
                    path.display()
                )
                .into());
            }
        }
    }
    Ok(tasks)
}

/// Run a subset of SWE-bench tasks against a caller-prepared repo.
///
/// v0.1 does not clone the repo nor apply `test_patch`; callers are expected
/// to do that themselves and point `options.repo` at the resulting workspace.
/// See `SWEBENCH.md` for the full integration plan.
pub fn run_swe_bench(options: SweBenchOptions) -> KimetsuResult<Vec<SweBenchInstanceResult>> {
    let mut tasks = read_tasks(&options.tasks)?;
    if let Some(instance_id) = options.instance_id.as_ref() {
        tasks.retain(|task| &task.instance_id == instance_id);
    }
    if let Some(limit) = options.limit {
        tasks.truncate(limit.min(tasks.len()));
    }

    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        let pipeline_task = format_task_brief(&task);
        let result: CodingRunResult = run_coding(CodingRunOptions {
            repo: options.repo.clone(),
            task: pipeline_task,
            dry_run: options.dry_run,
            allow_high_risk: true,
            disable_model: false,
            disable_broker: options.disable_broker,
            model_key_override: None,
        })?;
        results.push(SweBenchInstanceResult {
            instance_id: task.instance_id,
            repo: task.repo,
            run_id: result.run_id.to_string(),
            trace_path: result.trace_path,
            final_report_path: result.final_report_path,
            patch_plan_id: result.patch_plan_id,
            dry_run: result.dry_run,
            disable_broker: options.disable_broker,
        });
    }
    Ok(results)
}

fn format_task_brief(task: &SweBenchTask) -> String {
    let mut brief = format!(
        "SWE-bench instance: {}\nUpstream repo: {}\nBase commit: {}\n\nProblem:\n{}",
        task.instance_id, task.repo, task.base_commit, task.problem_statement
    );
    if let Some(hints) = task.hints_text.as_ref() {
        brief.push_str("\n\nHints:\n");
        brief.push_str(hints);
    }
    if let Some(fail_to_pass) = task.fail_to_pass.as_ref() {
        if !fail_to_pass.is_empty() {
            brief.push_str("\n\nMust pass after fix:\n");
            for case in fail_to_pass {
                brief.push_str(&format!("- {case}\n"));
            }
        }
    }
    brief
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_swe_bench_task() {
        let dir = std::env::temp_dir().join(format!(
            "kimetsu-swe-bench-test-{}",
            kimetsu_core::ids::new_id()
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("tasks.jsonl");
        let line = serde_json::json!({
            "instance_id": "django__django-12345",
            "repo": "django/django",
            "base_commit": "abc123",
            "problem_statement": "Fix the failing widget render."
        })
        .to_string();
        std::fs::write(&path, line).expect("write tasks");

        let tasks = read_tasks(&path).expect("read tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].instance_id, "django__django-12345");
        assert!(tasks[0].test_patch.is_none());

        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn formats_task_brief_with_hints() {
        let task = SweBenchTask {
            instance_id: "owner__repo-1".to_string(),
            repo: "owner/repo".to_string(),
            base_commit: "deadbeef".to_string(),
            problem_statement: "Bug: x does not work.".to_string(),
            hints_text: Some("Look at module x.".to_string()),
            test_patch: None,
            patch: None,
            version: None,
            fail_to_pass: Some(vec!["test_x_works".to_string()]),
            pass_to_pass: None,
        };
        let brief = format_task_brief(&task);
        assert!(brief.contains("owner__repo-1"));
        assert!(brief.contains("Hints:"));
        assert!(brief.contains("test_x_works"));
    }
}
