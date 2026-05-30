//! v0.4.6: `kimetsu doctor` — automated wire-health check.
//!
//! Validates that every kimetsu subsystem the chat REPL + MCP
//! sidecar rely on actually works, end-to-end, against the current
//! workspace and the user's `~/.kimetsu/` state. The checks are
//! hermetic by default — no real LLM calls, no network — so this
//! is safe to run in CI on every commit.
//!
//! Why this exists separately from `cargo test`:
//!   * unit tests prove the modules behave in isolation; doctor
//!     proves they're wired together correctly through the CLI +
//!     config + env paths a real user actually hits.
//!   * tests run against synthetic temp dirs; doctor runs against
//!     the REAL workspace + REAL user brain dir + REAL fastembed
//!     cache, catching environment-specific failures (perms,
//!     `KIMETSU_*` env overrides, partially-applied migrations).
//!
//! Check outcomes:
//!   * **Pass** — green, no action.
//!   * **Warn** — yellow, kimetsu still works but a follow-up step
//!     is recommended (e.g. "no embeddings feature → semantic
//!     retrieval is off").
//!   * **Fail** — red, kimetsu is misconfigured; exit code 1.
//!   * **Skip** — gray, the check doesn't apply (e.g. live-LLM
//!     skipped when no token).
//!
//! Exit codes:
//!   * 0 — all checks passed or warned.
//!   * 1 — at least one check failed.
//!   * 2 — internal doctor error (couldn't even run the checks).
//!
//! `--json` emits the report machine-readable for hook/CI consumers.

use std::path::{Path, PathBuf};

use kimetsu_brain::{ambient, embeddings, project, redact, user_brain};
use kimetsu_core::KimetsuResult;
use kimetsu_core::paths::ProjectPaths;
use serde::Serialize;

/// Per-check status.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Warn { reason: String },
    Fail { reason: String },
    Skip { reason: String },
}

impl Outcome {
    /// Convenience predicate for callers that aggregate per-check
    /// outcomes; not used internally but kept on the public API
    /// for downstream tooling (CI scripts, hooks).
    #[allow(dead_code)]
    pub fn is_fail(&self) -> bool {
        matches!(self, Outcome::Fail { .. })
    }
    pub fn glyph(&self) -> &'static str {
        match self {
            Outcome::Pass => "✓",
            Outcome::Warn { .. } => "⚠",
            Outcome::Fail { .. } => "✗",
            Outcome::Skip { .. } => "·",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub name: &'static str,
    pub category: &'static str,
    /// Inline outcome (Pass / Warn / Fail / Skip).
    pub outcome: Outcome,
    /// Optional one-line detail beyond the outcome reason — facts
    /// the user might want to see even on a Pass (counts, paths,
    /// detected branch).
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    /// Emit JSON instead of the human report. Used by CI + hooks.
    pub json: bool,
    /// Skip the MCP spawn check. Useful when running inside a
    /// sandbox where spawning is disallowed.
    pub skip_mcp: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub workspace: PathBuf,
    pub kimetsu_version: &'static str,
    pub embeddings_feature: bool,
    pub checks: Vec<CheckReport>,
    pub passed: usize,
    pub warned: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl DoctorReport {
    pub fn ok(&self) -> bool {
        self.failed == 0
    }
}

/// Entry point: run every check, return the consolidated report.
pub fn run(workspace: &Path, opts: DoctorOptions) -> KimetsuResult<DoctorReport> {
    let checks = vec![
        check_workspace_kimetsu_dir(workspace),
        check_project_brain_opens(workspace),
        check_user_brain_opens(),
        check_redact_smoke(),
        check_ambient_collect(workspace),
        check_embedder_default(),
        check_mcp_tools_advertised(workspace, opts.skip_mcp),
        check_hooks_installed(workspace),
    ];

    let mut passed = 0;
    let mut warned = 0;
    let mut failed = 0;
    let mut skipped = 0;
    for check in &checks {
        match check.outcome {
            Outcome::Pass => passed += 1,
            Outcome::Warn { .. } => warned += 1,
            Outcome::Fail { .. } => failed += 1,
            Outcome::Skip { .. } => skipped += 1,
        }
    }

    Ok(DoctorReport {
        workspace: workspace.to_path_buf(),
        kimetsu_version: env!("CARGO_PKG_VERSION"),
        embeddings_feature: embeddings_feature_enabled(),
        checks,
        passed,
        warned,
        failed,
        skipped,
    })
}

/// Pretty-print a report to stdout. JSON output is handled by the
/// caller via `serde_json::to_string_pretty`.
pub fn print_human(report: &DoctorReport) {
    println!(
        "[doctor] kimetsu v{} (fastembed={})",
        report.kimetsu_version,
        if report.embeddings_feature {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("[doctor] workspace: {}", report.workspace.display());
    println!();
    let mut current_category: Option<&'static str> = None;
    for check in &report.checks {
        if current_category != Some(check.category) {
            current_category = Some(check.category);
            println!("  {}:", check.category);
        }
        let glyph = check.outcome.glyph();
        let suffix = match &check.outcome {
            Outcome::Pass => String::new(),
            Outcome::Warn { reason } => format!(" — {reason}"),
            Outcome::Fail { reason } => format!(" — {reason}"),
            Outcome::Skip { reason } => format!(" — {reason}"),
        };
        match &check.detail {
            Some(d) => println!("    {glyph}  {} ({d}){suffix}", check.name),
            None => println!("    {glyph}  {}{suffix}", check.name),
        }
    }
    println!();
    println!(
        "  {} passed, {} warned, {} failed, {} skipped",
        report.passed, report.warned, report.failed, report.skipped
    );
    if !report.ok() {
        println!("\n[doctor] FAIL — fix the items above and re-run `kimetsu doctor`.");
    } else if report.warned > 0 {
        println!("\n[doctor] OK with warnings — kimetsu works but consider the items above.");
    } else {
        println!("\n[doctor] OK — kimetsu is healthy.");
    }
}

// --------- individual checks ---------

fn check_workspace_kimetsu_dir(workspace: &Path) -> CheckReport {
    let paths = match ProjectPaths::discover(workspace) {
        Ok(p) => p,
        Err(err) => {
            return CheckReport {
                name: ".kimetsu/ directory present",
                category: "workspace",
                outcome: Outcome::Warn {
                    reason: format!(
                        "discovery failed: {err}; run `kimetsu init` to create project state"
                    ),
                },
                detail: None,
            };
        }
    };
    if paths.kimetsu_dir.exists() {
        CheckReport {
            name: ".kimetsu/ directory present",
            category: "workspace",
            outcome: Outcome::Pass,
            detail: Some(paths.kimetsu_dir.display().to_string()),
        }
    } else {
        CheckReport {
            name: ".kimetsu/ directory present",
            category: "workspace",
            outcome: Outcome::Warn {
                reason: "no .kimetsu/ found — chat works but project-scoped brain is unavailable. Run `kimetsu init`.".into(),
            },
            detail: None,
        }
    }
}

fn check_project_brain_opens(workspace: &Path) -> CheckReport {
    match project::list_memories(workspace) {
        Ok(memories) => CheckReport {
            name: "project brain.db opens",
            category: "brain",
            outcome: Outcome::Pass,
            detail: Some(format!("{} active memories", memories.len())),
        },
        Err(err) => {
            // Not initialized is a warning, not a fail — chat still
            // works without a project brain.
            let msg = err.to_string();
            if msg.contains(".kimetsu") || msg.contains("project.toml") {
                CheckReport {
                    name: "project brain.db opens",
                    category: "brain",
                    outcome: Outcome::Warn {
                        reason: "no project initialized — run `kimetsu init`".into(),
                    },
                    detail: None,
                }
            } else {
                CheckReport {
                    name: "project brain.db opens",
                    category: "brain",
                    outcome: Outcome::Fail { reason: msg },
                    detail: None,
                }
            }
        }
    }
}

fn check_user_brain_opens() -> CheckReport {
    match user_brain::open_user_brain_readonly() {
        Ok(Some(conn)) => {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            CheckReport {
                name: "user brain.db opens",
                category: "brain",
                outcome: Outcome::Pass,
                detail: Some(format!(
                    "{} active memories at {}",
                    count,
                    user_brain::user_brain_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<unresolved>".to_string())
                )),
            }
        }
        Ok(None) => CheckReport {
            name: "user brain.db opens",
            category: "brain",
            outcome: Outcome::Skip {
                reason: "user brain disabled (KIMETSU_USER_BRAIN=0) or file not yet created".into(),
            },
            detail: None,
        },
        Err(err) => CheckReport {
            name: "user brain.db opens",
            category: "brain",
            outcome: Outcome::Fail {
                reason: err.to_string(),
            },
            detail: None,
        },
    }
}

fn check_redact_smoke() -> CheckReport {
    // Drive the redactor against a known secret-laden test string so
    // doctor catches a broken regex set even if no live secret is
    // ever encountered.
    let sample = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf and ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ";
    let r = redact::redact_secrets(sample);
    let kinds: Vec<&str> = r.matches.iter().map(|m| m.kind).collect();
    let want_anthropic = kinds.contains(&"anthropic_oauth");
    let want_github = kinds.contains(&"github_pat");
    if want_anthropic && want_github {
        CheckReport {
            name: "secret redaction patterns active",
            category: "safety",
            outcome: Outcome::Pass,
            detail: Some(format!("{} match(es) on the smoke string", r.matches.len())),
        }
    } else {
        CheckReport {
            name: "secret redaction patterns active",
            category: "safety",
            outcome: Outcome::Fail {
                reason: format!(
                    "smoke string did not redact as expected. anthropic={want_anthropic} github={want_github}"
                ),
            },
            detail: None,
        }
    }
}

fn check_ambient_collect(workspace: &Path) -> CheckReport {
    if !ambient::ambient_enabled() {
        return CheckReport {
            name: "ambient context collection",
            category: "retrieval",
            outcome: Outcome::Skip {
                reason: "disabled via KIMETSU_BRAIN_AMBIENT".into(),
            },
            detail: None,
        };
    }
    let ctx = ambient::collect(workspace);
    let suffix = ambient::render_as_query_suffix(&ctx);
    if suffix.is_empty() {
        CheckReport {
            name: "ambient context collection",
            category: "retrieval",
            outcome: Outcome::Warn {
                reason: "collected but empty — not a git repo or no recent files".into(),
            },
            detail: None,
        }
    } else {
        CheckReport {
            name: "ambient context collection",
            category: "retrieval",
            outcome: Outcome::Pass,
            detail: Some(format!(
                "branch={} dirty={} recent={}",
                ctx.branch.as_deref().unwrap_or("<none>"),
                ctx.git_status.len(),
                ctx.recent_files.len()
            )),
        }
    }
}

fn check_embedder_default() -> CheckReport {
    let embedder = embeddings::open_default_embedder();
    if embedder.is_noop() {
        if embeddings_feature_enabled() {
            CheckReport {
                name: "default embedder loads",
                category: "retrieval",
                outcome: Outcome::Warn {
                    reason: "`embeddings` feature is built in but the embedder fell back to Noop — check stderr for fastembed init errors or unset KIMETSU_BRAIN_EMBEDDER=noop".into(),
                },
                detail: None,
            }
        } else {
            CheckReport {
                name: "default embedder loads",
                category: "retrieval",
                outcome: Outcome::Warn {
                    reason: "no `embeddings` feature - semantic retrieval off. Reinstall with `cargo install kimetsu-cli` for the default semantic build.".into(),
                },
                detail: Some("NoopEmbedder (FTS-only retrieval)".into()),
            }
        }
    } else {
        CheckReport {
            name: "default embedder loads",
            category: "retrieval",
            outcome: Outcome::Pass,
            detail: Some(format!("{} ({} dim)", embedder.model_id(), embedder.dim())),
        }
    }
}

fn check_mcp_tools_advertised(_workspace: &Path, skip: bool) -> CheckReport {
    if skip {
        return CheckReport {
            name: "MCP tools/list advertises ≥16 kimetsu_* tools",
            category: "mcp",
            outcome: Outcome::Skip {
                reason: "--skip-mcp set".into(),
            },
            detail: None,
        };
    }
    // The MCP server's tool catalog is built statically inside
    // kimetsu-chat — calling it here without spawning a subprocess
    // would require importing kimetsu-chat as a doctor dep. For
    // v0.4.6 first cut we report a Skip and document that the live
    // tools/list smoke runs via `kimetsu mcp serve` in CI. A
    // follow-up commit will wire the real spawn check.
    CheckReport {
        name: "MCP tools/list advertises ≥16 kimetsu_* tools",
        category: "mcp",
        outcome: Outcome::Skip {
            reason: "v0.4.6 first cut — spawn check lands in v0.4.6.1. The 16-tool catalog is covered by kimetsu-chat unit tests today.".into(),
        },
        detail: None,
    }
}

fn check_hooks_installed(workspace: &Path) -> CheckReport {
    let mut found = Vec::new();

    let claude_settings = workspace.join(".claude").join("settings.json");
    if file_contains_all(
        &claude_settings,
        &["UserPromptSubmit", "kimetsu brain context-hook"],
    ) {
        found.push(".claude/settings.json");
    }

    let codex_hooks = workspace.join(".codex").join("hooks.json");
    if file_contains_all(
        &codex_hooks,
        &["UserPromptSubmit", "kimetsu brain context-hook"],
    ) {
        found.push(".codex/hooks.json");
    }

    if found.is_empty() {
        CheckReport {
            name: "host brain hook installed",
            category: "plugin",
            outcome: Outcome::Skip {
                reason: "no Claude/Codex brain hook config found - run `kimetsu plugin install claude` or `kimetsu plugin install codex` to enable prompt-time brain injection".into(),
            },
            detail: None,
        }
    } else {
        CheckReport {
            name: "host brain hook installed",
            category: "plugin",
            outcome: Outcome::Pass,
            detail: Some(format!("{} hook(s): {}", found.len(), found.join(", "))),
        }
    }
}

fn file_contains_all(path: &Path, needles: &[&str]) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    needles.iter().all(|needle| text.contains(needle))
}

fn embeddings_feature_enabled() -> bool {
    // The `embeddings` feature on kimetsu-brain is the only way
    // open_default_embedder returns a non-Noop embedder. We can't
    // observe the feature flag of a dependency directly, so we
    // approximate: if the default embedder is non-noop, the feature
    // must have been compiled in.
    let e = embeddings::open_default_embedder();
    !e.is_noop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_smoke_passes_against_known_secret_string() {
        let r = check_redact_smoke();
        assert!(matches!(r.outcome, Outcome::Pass), "{r:?}");
    }

    #[test]
    fn ambient_collect_handles_non_git_dir_gracefully() {
        // Run against a temp dir that's NOT a git repo — collection
        // should still succeed but report empty signals.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!("kimetsu-doctor-test-{nanos}"));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        std::fs::write(tmp.join("note.txt"), "hello").expect("write");
        let r = check_ambient_collect(&tmp);
        // Expect Pass (because a recent file was created) OR Warn
        // (if KIMETSU_BRAIN_AMBIENT=off in this test process). Both
        // are legitimate outcomes.
        match r.outcome {
            Outcome::Pass | Outcome::Warn { .. } | Outcome::Skip { .. } => {}
            Outcome::Fail { ref reason } => panic!("non-git workspace should not fail: {reason}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn doctor_report_aggregates_counts() {
        let report = DoctorReport {
            workspace: PathBuf::from("/test"),
            kimetsu_version: "0.0.0",
            embeddings_feature: false,
            checks: vec![
                CheckReport {
                    name: "a",
                    category: "x",
                    outcome: Outcome::Pass,
                    detail: None,
                },
                CheckReport {
                    name: "b",
                    category: "x",
                    outcome: Outcome::Warn { reason: "w".into() },
                    detail: None,
                },
                CheckReport {
                    name: "c",
                    category: "y",
                    outcome: Outcome::Fail { reason: "f".into() },
                    detail: None,
                },
                CheckReport {
                    name: "d",
                    category: "y",
                    outcome: Outcome::Skip { reason: "s".into() },
                    detail: None,
                },
            ],
            passed: 1,
            warned: 1,
            failed: 1,
            skipped: 1,
        };
        assert!(!report.ok(), "1 fail means !ok");
    }

    #[test]
    fn outcome_glyphs_distinct() {
        // Doctor's human output relies on these being different per
        // status. Avoid a typo regression that prints the same
        // glyph for two states.
        let pass = Outcome::Pass.glyph();
        let warn = Outcome::Warn {
            reason: String::new(),
        }
        .glyph();
        let fail = Outcome::Fail {
            reason: String::new(),
        }
        .glyph();
        let skip = Outcome::Skip {
            reason: String::new(),
        }
        .glyph();
        assert_ne!(pass, warn);
        assert_ne!(pass, fail);
        assert_ne!(pass, skip);
        assert_ne!(warn, fail);
        assert_ne!(warn, skip);
        assert_ne!(fail, skip);
    }

    #[test]
    fn doctor_detects_current_codex_hooks_json() {
        let tmp = tempdir_in_test("kimetsu-doctor-codex-hooks");
        let codex_dir = tmp.join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("mkdir");
        std::fs::write(
            codex_dir.join("hooks.json"),
            r#"{
              "hooks": {
                "UserPromptSubmit": [{
                  "hooks": [{
                    "type": "command",
                    "command": "kimetsu brain context-hook --workspace ."
                  }]
                }]
              }
            }"#,
        )
        .expect("write hooks");

        let report = check_hooks_installed(&tmp);
        assert!(matches!(report.outcome, Outcome::Pass), "{report:?}");
        assert!(
            report
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains(".codex/hooks.json")
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn doctor_rejects_legacy_pre_turn_scripts() {
        let tmp = tempdir_in_test("kimetsu-doctor-legacy-hooks");
        let legacy_dir = tmp.join(".codex").join("hooks");
        std::fs::create_dir_all(&legacy_dir).expect("mkdir");
        std::fs::write(
            legacy_dir.join("pre-turn.ps1"),
            "kimetsu brain context-hook --workspace .",
        )
        .expect("write legacy hook");

        let report = check_hooks_installed(&tmp);
        assert!(matches!(report.outcome, Outcome::Skip { .. }), "{report:?}");
        let _ = std::fs::remove_dir_all(tmp);
    }

    fn tempdir_in_test(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let tmp = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        tmp
    }
}
