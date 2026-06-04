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
use std::time::SystemTime;

use kimetsu_brain::{ambient, embeddings, project, redact, user_brain};
use kimetsu_core::KimetsuResult;
use kimetsu_core::paths::ProjectPaths;
use serde::Serialize;

use crate::process::{KimetsuProc, ProcKind};

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
        check_running_mcp_servers(),
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
            } else if is_schema_mismatch(&msg) {
                CheckReport {
                    name: "project brain.db opens",
                    category: "brain",
                    outcome: Outcome::Fail {
                        reason: format!(
                            "{msg} — if a host MCP server (Claude Code / Codex) is running an \
                             older kimetsu, restart it so it picks up the new binary and schema"
                        ),
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
        Err(err) => {
            let msg = err.to_string();
            let reason = if is_schema_mismatch(&msg) {
                format!(
                    "{msg} — if a host MCP server (Claude Code / Codex) is running an \
                     older kimetsu, restart it so it picks up the new binary and schema"
                )
            } else {
                msg
            };
            CheckReport {
                name: "user brain.db opens",
                category: "brain",
                outcome: Outcome::Fail { reason },
                detail: None,
            }
        }
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

/// Returns true when an error message indicates a brain.db schema mismatch.
fn is_schema_mismatch(msg: &str) -> bool {
    msg.contains("schema version") || msg.contains("SchemaNeedsMigration")
}

/// Assess whether running MCP servers are stale relative to the on-disk binary.
///
/// Pure decision function — takes synthetic inputs so it can be unit-tested
/// without touching any live OS state.
///
/// Parameters:
/// - `servers`: list of `(started_at, exe_path)` tuples for each MCP server.
///   `started_at` is seconds since epoch; `exe_path` is the path of the running
///   binary as reported by the OS.
/// - `binary_mtime`: modification time of the current on-disk binary, in
///   seconds since epoch.
/// - `binary_path`: canonical path of the current binary (for exe-path comparison).
///
/// Returns the highest-severity outcome across all servers.
pub fn assess_mcp_skew(
    servers: &[(u32, Option<u64>, Option<&str>)], // (pid, started_at, exe_path)
    binary_mtime: Option<u64>,
    binary_path: Option<&str>,
) -> Outcome {
    if servers.is_empty() {
        return Outcome::Pass;
    }

    let restart_hint =
        "restart your host agent (Claude Code / Codex) so it respawns the MCP server";

    let mut stale_pids: Vec<String> = Vec::new();
    let mut wrong_exe_pids: Vec<String> = Vec::new();
    let mut unknown_count = 0usize;

    for &(pid, started_at, exe_path) in servers {
        let mut flagged = false;

        // Check 1: exe_path mismatch (running a different binary on disk).
        if let (Some(running_exe), Some(current_exe)) = (exe_path, binary_path) {
            let running_lower = running_exe.to_lowercase();
            let current_lower = current_exe.to_lowercase();
            if running_lower != current_lower {
                wrong_exe_pids.push(format!(
                    "PID {pid} (exe: {running_exe}; current: {current_exe})"
                ));
                flagged = true;
            }
        }

        // Check 2: start time vs binary mtime — server predates the new binary.
        if !flagged {
            match (started_at, binary_mtime) {
                (Some(started), Some(mtime)) => {
                    if started < mtime {
                        stale_pids.push(format!("PID {pid}"));
                        flagged = true;
                    }
                }
                _ => {
                    if !flagged {
                        unknown_count += 1;
                    }
                }
            }
        }

        let _ = flagged; // suppress unused-assignment warning
    }

    if !stale_pids.is_empty() || !wrong_exe_pids.is_empty() {
        let mut parts = Vec::new();
        if !stale_pids.is_empty() {
            parts.push(format!(
                "{} kimetsu MCP server(s) started before the current binary was written \
                 ({}) — they are running a stale version and may fail with a brain schema \
                 mismatch. {}.",
                stale_pids.len(),
                stale_pids.join(", "),
                restart_hint,
            ));
        }
        if !wrong_exe_pids.is_empty() {
            parts.push(format!(
                "{} kimetsu MCP server(s) are running from a different binary path than \
                 the current one ({}) — {}.",
                wrong_exe_pids.len(),
                wrong_exe_pids.join(", "),
                restart_hint,
            ));
        }
        Outcome::Warn {
            reason: parts.join(" "),
        }
    } else if unknown_count > 1 {
        // Multiple servers and we can't tell if they're fresh — be informational.
        Outcome::Warn {
            reason: format!(
                "{unknown_count} kimetsu MCP server(s) running; if you recently updated or \
                 reinstalled kimetsu, {restart_hint}.",
            ),
        }
    } else {
        // Single server or zero unknowns — can't confirm staleness, but no evidence of a problem.
        Outcome::Pass
    }
}

/// Check whether any running kimetsu MCP server processes are stale (started
/// before the current binary was written to disk).
///
/// A stale MCP server is the most common cause of the cryptic
/// "brain schema mismatch" error after a kimetsu update — the host agent
/// keeps running the old binary image until the user restarts it.
fn check_running_mcp_servers() -> CheckReport {
    const NAME: &str = "running MCP servers up to date";
    const CATEGORY: &str = "process";

    // Get the modification time of the current binary (best-effort).
    let binary_mtime: Option<u64> = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let binary_path: Option<String> = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok().or(Some(p)))
        .map(|p| p.to_string_lossy().to_lowercase());

    // List all running kimetsu processes (excludes self).
    let all_procs: Vec<KimetsuProc> = crate::process::list_kimetsu_processes();
    let mcp_servers: Vec<&KimetsuProc> = all_procs
        .iter()
        .filter(|p| p.kind == ProcKind::McpServe)
        .collect();

    if mcp_servers.is_empty() {
        return CheckReport {
            name: NAME,
            category: CATEGORY,
            outcome: Outcome::Pass,
            detail: Some("no running kimetsu MCP servers".into()),
        };
    }

    let server_count = mcp_servers.len();

    // Build the input for assess_mcp_skew.
    let servers: Vec<(u32, Option<u64>, Option<&str>)> = mcp_servers
        .iter()
        .map(|p| (p.pid, p.started_at, p.exe_path.as_deref()))
        .collect();

    let outcome = assess_mcp_skew(&servers, binary_mtime, binary_path.as_deref());

    // Build a detail line with the list of running MCP server PIDs.
    let pid_list: Vec<String> = mcp_servers
        .iter()
        .map(|p| {
            let ws = p.workspace.as_deref().unwrap_or("-");
            format!("PID {} (workspace: {ws})", p.pid)
        })
        .collect();
    let detail = Some(format!("{server_count} server(s): {}", pid_list.join(", ")));

    CheckReport {
        name: NAME,
        category: CATEGORY,
        outcome,
        detail,
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

/// D4: `kimetsu doctor --selftest` — hermetic end-to-end round-trip.
///
/// Exercises the full record → retrieve path in a throw-away temp project:
///
/// 1. Create an isolated temp dir with its own `git init` boundary.
/// 2. Init a kimetsu project there.
/// 3. Record a sample memory with a distinctive text.
/// 4. Query the brain for that text via FTS retrieval.
/// 5. Confirm the memory comes back (matched by substring).
/// 6. Print "✓ recorded a memory and retrieved it — the brain works".
/// 7. Delete the temp dir.
///
/// Works on both lean (FTS-only) and `--features embeddings` builds.
/// Does NOT touch the real workspace brain or user brain.
/// Exits non-zero (returns `Err`) on any failure.
pub fn run_selftest() -> KimetsuResult<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = std::env::temp_dir().join(format!("kimetsu-selftest-{nanos}"));
    std::fs::create_dir_all(&tmp)?;
    kimetsu_core::paths::git_init_boundary(&tmp);

    let result = run_selftest_in(&tmp);

    // Always clean up, even on failure.
    let _ = std::fs::remove_dir_all(&tmp);

    match result {
        Ok(()) => {
            println!("✓ recorded a memory and retrieved it — the brain works");
            Ok(())
        }
        Err(err) => Err(format!("kimetsu doctor --selftest FAILED: {err}").into()),
    }
}

fn run_selftest_in(root: &Path) -> KimetsuResult<()> {
    // Disable user-brain so the selftest never touches ~/.kimetsu.
    kimetsu_brain::user_brain::with_user_brain_disabled(|| run_selftest_isolated(root))
}

fn run_selftest_isolated(root: &Path) -> KimetsuResult<()> {
    use kimetsu_brain::project as bp;
    use kimetsu_core::memory::{MemoryKind, MemoryScope};

    // 1. Init project.
    bp::init_project(root, false).map_err(|e| format!("selftest: init_project failed: {e}"))?;

    // 2. Record a distinctive memory.
    let probe = "kimetsu-selftest-probe-xq7w9";
    bp::add_memory(root, MemoryScope::Project, MemoryKind::Fact, probe)
        .map_err(|e| format!("selftest: add_memory failed: {e}"))?;

    // 3. Retrieve via FTS — must find the probe text.
    let hits = bp::search_memories(root, probe, 5, 0, None, None)
        .map_err(|e| format!("selftest: search_memories failed: {e}"))?;

    if hits.iter().any(|h| h.text.contains(probe)) {
        Ok(())
    } else {
        Err(format!(
            "selftest: recorded memory `{probe}` not found in retrieval results ({} hits)",
            hits.len()
        )
        .into())
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

    /// D4: `--selftest` must exit 0 on a healthy setup and print
    /// the success line. Runs hermetically in a throw-away temp dir.
    #[test]
    fn selftest_passes_on_healthy_setup() {
        // run_selftest_in exercises record → retrieve without touching
        // the real workspace brain (user brain disabled inside the
        // helper via with_user_brain_disabled).
        let tmp = tempdir_in_test("kimetsu-doctor-selftest");
        kimetsu_core::paths::git_init_boundary(&tmp);
        let result = run_selftest_in(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            result.is_ok(),
            "selftest should pass on a clean temp project: {result:?}"
        );
    }

    // ── assess_mcp_skew pure-function unit tests ─────────────────────────────

    /// No MCP servers → always Pass.
    #[test]
    fn skew_no_servers_is_pass() {
        let outcome = assess_mcp_skew(&[], Some(1000), Some("/usr/bin/kimetsu"));
        assert!(matches!(outcome, Outcome::Pass), "{outcome:?}");
    }

    /// A single fresh server (started AFTER the binary mtime) → Pass.
    #[test]
    fn skew_fresh_server_is_pass() {
        let binary_mtime = 1_000_000u64;
        let started_after = binary_mtime + 60; // started 60s after binary was written
        let servers = [(1001u32, Some(started_after), Some("/usr/bin/kimetsu"))];
        let outcome = assess_mcp_skew(&servers, Some(binary_mtime), Some("/usr/bin/kimetsu"));
        assert!(
            matches!(outcome, Outcome::Pass),
            "fresh server must not produce a warning: {outcome:?}"
        );
    }

    /// A server that started BEFORE the binary mtime → Warn with restart guidance.
    #[test]
    fn skew_stale_server_is_warn_with_restart_guidance() {
        let binary_mtime = 1_000_000u64;
        let started_before = binary_mtime - 60; // started 60s BEFORE binary was updated
        let servers = [(1001u32, Some(started_before), Some("/usr/bin/kimetsu"))];
        let outcome = assess_mcp_skew(&servers, Some(binary_mtime), Some("/usr/bin/kimetsu"));
        match &outcome {
            Outcome::Warn { reason } => {
                assert!(reason.contains("1001"), "warn should mention the PID");
                assert!(
                    reason.to_lowercase().contains("restart"),
                    "warn should mention restart: {reason}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    /// A server running from a different binary path → Warn.
    #[test]
    fn skew_wrong_exe_path_is_warn() {
        let binary_mtime = 1_000_000u64;
        let started_after = binary_mtime + 60;
        // Running from a different path (e.g. old location).
        let servers = [(2002u32, Some(started_after), Some("/old/path/kimetsu"))];
        let outcome = assess_mcp_skew(&servers, Some(binary_mtime), Some("/usr/local/bin/kimetsu"));
        match &outcome {
            Outcome::Warn { reason } => {
                assert!(
                    reason.contains("2002") || reason.to_lowercase().contains("binary path"),
                    "warn should mention the PID or path issue: {reason}"
                );
            }
            other => panic!("expected Warn for wrong exe path, got {other:?}"),
        }
    }

    /// No start time available, single server → Pass (can't confirm staleness,
    /// but single server is not worth spamming warnings).
    #[test]
    fn skew_single_server_no_start_time_is_pass() {
        let servers = [(3003u32, None, Some("/usr/bin/kimetsu"))];
        let outcome = assess_mcp_skew(&servers, Some(1_000_000), Some("/usr/bin/kimetsu"));
        assert!(
            matches!(outcome, Outcome::Pass),
            "single unknown-start server should be Pass: {outcome:?}"
        );
    }

    /// Multiple servers with no start time → informational Warn.
    #[test]
    fn skew_multiple_servers_no_start_time_is_warn() {
        let servers = [
            (4001u32, None, Some("/usr/bin/kimetsu")),
            (4002u32, None, Some("/usr/bin/kimetsu")),
        ];
        let outcome = assess_mcp_skew(&servers, Some(1_000_000), Some("/usr/bin/kimetsu"));
        assert!(
            matches!(outcome, Outcome::Warn { .. }),
            "multiple servers with unknown start time should Warn: {outcome:?}"
        );
    }

    /// No binary mtime available → falls through to Pass (can't determine staleness).
    #[test]
    fn skew_no_binary_mtime_single_server_is_pass() {
        let servers = [(5001u32, Some(999_999), Some("/usr/bin/kimetsu"))];
        // binary_mtime is None → can't compare → unknown_count=1 → Pass
        let outcome = assess_mcp_skew(&servers, None, Some("/usr/bin/kimetsu"));
        assert!(
            matches!(outcome, Outcome::Pass),
            "no binary mtime, single server → Pass: {outcome:?}"
        );
    }

    /// Mixed: one stale and one fresh server → Warn (highest severity wins).
    #[test]
    fn skew_mixed_stale_and_fresh_is_warn() {
        let binary_mtime = 1_000_000u64;
        let servers = [
            (6001u32, Some(binary_mtime - 60), Some("/usr/bin/kimetsu")), // stale
            (6002u32, Some(binary_mtime + 60), Some("/usr/bin/kimetsu")), // fresh
        ];
        let outcome = assess_mcp_skew(&servers, Some(binary_mtime), Some("/usr/bin/kimetsu"));
        match &outcome {
            Outcome::Warn { reason } => {
                assert!(
                    reason.contains("6001"),
                    "stale PID 6001 should be called out: {reason}"
                );
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    /// Schema mismatch error messages must include the restart hint.
    #[test]
    fn schema_mismatch_detection() {
        assert!(
            is_schema_mismatch("brain.db schema version 1 is older than this binary's 2"),
            "schema version message should be detected"
        );
        assert!(
            is_schema_mismatch("SchemaNeedsMigration"),
            "SchemaNeedsMigration type name should be detected"
        );
        assert!(
            !is_schema_mismatch("no .kimetsu directory found"),
            "unrelated error must not be flagged as schema mismatch"
        );
    }
}
