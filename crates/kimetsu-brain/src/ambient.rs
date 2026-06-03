//! v0.4.4: ambient workspace context for the pre-turn hook.
//!
//! The MCP `kimetsu_brain_context` and `kimetsu_benchmark_context`
//! tools (and the equivalent `kimetsu brain context` CLI) used to
//! depend entirely on the caller producing a useful `query`. When the
//! user types "fix it" or "continue", the host harness has nothing to
//! disambiguate — and the brain has no way to surface the right
//! capsules.
//!
//! This module bridges that gap by collecting a small, cheap
//! workspace fingerprint on every retrieval call:
//!   * Current git branch (`git rev-parse --abbrev-ref HEAD`).
//!   * Working-tree dirty state (`git status --short`, top entries).
//!   * Recently-modified files in the repo (mtime descending, top 5,
//!     ignoring `.git`, `node_modules`, `target`, etc. via the same
//!     `ignore` crate the ingest pipeline already uses).
//!
//! [`render_as_query_suffix`] formats those into a short, lexically-
//! AND-semantically retrievable suffix:
//!
//! ```text
//! [workspace: branch=feature/embedder | recent: src/embeddings.rs,
//!  src/context.rs, src/project.rs | dirty: M src/embeddings.rs,
//!  ?? src/ambient.rs]
//! ```
//!
//! v0.4.3's semantic retrieval makes the suffix valuable beyond
//! keyword overlap: branch names, file paths, and dirty-line text
//! all embed and contribute cosine signal alongside the explicit
//! query.
//!
//! All collection is best-effort: a missing git binary, a non-git
//! workspace, or a permission error yields an empty field, not an
//! error. The whole module is opt-out via
//! `KIMETSU_BRAIN_AMBIENT=off` (or `0`/`false`/`no`/`none`), and a
//! per-call `include_ambient=false` parameter overrides per call.
//!
//! Cost budget: collection runs once per pre-turn hook fire. Target:
//! <50ms on a warm repo with up to 50k tracked files. Git calls have
//! a 2-second hard timeout.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Per-call ambient context snapshot. Serialized as part of the MCP
/// response so callers can inspect what augmented their query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmbientContext {
    /// Current git branch, e.g. "main" or "feature/x". `None` if not
    /// in a git repo, git is missing, or the call timed out.
    pub branch: Option<String>,
    /// Top N entries of `git status --short`. Empty when clean OR
    /// when git isn't available.
    pub git_status: Vec<StatusEntry>,
    /// Up to N recently-modified files (relative paths, mtime
    /// descending). Filtered via the ingest's `ignore` rules so
    /// `target/` / `node_modules/` / `.git/` don't dominate.
    pub recent_files: Vec<PathBuf>,
    /// UTC unix timestamp the snapshot was taken at. Used by
    /// callers that want to age out stale fingerprints.
    pub collected_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusEntry {
    /// First two columns of `git status --short` (e.g. " M", "??",
    /// "A ", "MM"). Preserved verbatim so callers can render git's
    /// own glyphs in tooltips.
    pub flag: String,
    pub path: PathBuf,
}

/// Tunables. Defaults match the per-turn budget.
#[derive(Debug, Clone, Copy)]
pub struct CollectOptions {
    /// Cap on the number of recent_files returned.
    pub recent_files_limit: usize,
    /// Cap on the number of git_status entries returned.
    pub git_status_limit: usize,
    /// Per-shell-out timeout for git invocations.
    pub git_timeout: Duration,
    /// Hard cap on filesystem walk duration.
    pub walk_budget: Duration,
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self {
            recent_files_limit: 5,
            git_status_limit: 8,
            git_timeout: Duration::from_secs(2),
            walk_budget: Duration::from_millis(150),
        }
    }
}

/// v0.4.4: opt-out gate. Reads `KIMETSU_BRAIN_AMBIENT`. Truthy or
/// unset → enabled (default); "off"/"0"/"false"/"no"/"none"
/// (case-insensitive) → disabled. Tests respect this so the brain
/// test isolation pattern can keep ambient collection off when
/// asserting on deterministic outputs.
pub fn ambient_enabled() -> bool {
    match std::env::var("KIMETSU_BRAIN_AMBIENT") {
        Ok(value) => {
            let v = value.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "off" | "0" | "false" | "no" | "none")
        }
        Err(_) => true,
    }
}

/// Collect the ambient context for `workspace`. Never errors —
/// returns a default-empty struct on any sub-collection failure.
pub fn collect(workspace: &Path) -> AmbientContext {
    collect_with_opts(workspace, &CollectOptions::default())
}

pub fn collect_with_opts(workspace: &Path, opts: &CollectOptions) -> AmbientContext {
    let collected_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    AmbientContext {
        branch: collect_branch(workspace, opts.git_timeout),
        git_status: collect_git_status(workspace, opts.git_status_limit, opts.git_timeout),
        recent_files: collect_recent_files(workspace, opts.recent_files_limit, opts.walk_budget),
        collected_at_unix,
    }
}

/// Render the snapshot as a one-line suffix to append to the query
/// before retrieval. Empty fields are omitted so the suffix stays
/// readable. Returns an empty string when the snapshot has nothing
/// useful to surface (no branch, clean tree, no recent files) —
/// callers can then skip the augmentation entirely.
pub fn render_as_query_suffix(ctx: &AmbientContext) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(branch) = ctx.branch.as_deref().filter(|s| !s.is_empty()) {
        parts.push(format!("branch={branch}"));
    }
    if !ctx.recent_files.is_empty() {
        let listed = ctx
            .recent_files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("recent: {listed}"));
    }
    if !ctx.git_status.is_empty() {
        let dirty = ctx
            .git_status
            .iter()
            .map(|entry| {
                format!(
                    "{} {}",
                    entry.flag.trim(),
                    entry.path.to_string_lossy().replace('\\', "/")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("dirty: {dirty}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\n[workspace: {}]", parts.join(" | "))
    }
}

/// Convenience: combine `query` with `render_as_query_suffix(ctx)`.
/// When the suffix is empty, the query is returned unchanged.
pub fn augment_query(query: &str, ctx: &AmbientContext) -> String {
    let suffix = render_as_query_suffix(ctx);
    if suffix.is_empty() {
        query.to_string()
    } else {
        format!("{query}{suffix}")
    }
}

// --------- internals ---------

fn collect_branch(workspace: &Path, timeout: Duration) -> Option<String> {
    let out = run_git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"], timeout)?;
    let trimmed = out.trim();
    if trimmed.is_empty() || trimmed == "HEAD" {
        // "HEAD" is git's name for a detached-head state; surface
        // it as None so the suffix doesn't claim a branch we don't
        // actually have.
        return None;
    }
    Some(trimmed.to_string())
}

fn collect_git_status(workspace: &Path, limit: usize, timeout: Duration) -> Vec<StatusEntry> {
    let Some(out) = run_git(workspace, &["status", "--short", "--no-renames"], timeout) else {
        return Vec::new();
    };
    parse_git_status(&out, limit)
}

fn parse_git_status(stdout: &str, limit: usize) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    for line in stdout.lines() {
        if entries.len() >= limit {
            break;
        }
        // git status --short format is `XY<space>path` — two
        // status columns, then a space, then the path. Reject
        // lines that don't have a space at column 2 so free-form
        // garbage ("badly-formatted") doesn't accidentally parse
        // as ("ba", "ly-formatted").
        let bytes = line.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            continue;
        }
        let flag = line.get(..2).unwrap_or("").to_string();
        let rest = line.get(3..).unwrap_or("").trim();
        if rest.is_empty() {
            continue;
        }
        entries.push(StatusEntry {
            flag,
            path: PathBuf::from(rest),
        });
    }
    entries
}

fn collect_recent_files(workspace: &Path, limit: usize, budget: Duration) -> Vec<PathBuf> {
    if limit == 0 {
        return Vec::new();
    }
    let started = Instant::now();
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    let walker = ignore::WalkBuilder::new(workspace)
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .max_depth(Some(6))
        .build();
    for result in walker {
        if started.elapsed() > budget {
            break;
        }
        let Ok(entry) = result else { continue };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let abs = entry.path().to_path_buf();
        let rel = abs.strip_prefix(workspace).unwrap_or(&abs).to_path_buf();
        // Skip files inside .kimetsu/ — they're framework state, not
        // user code, and noisy mtimes (every run rewrites trace
        // files) would dominate the list.
        if rel
            .components()
            .next()
            .map(|c| c.as_os_str() == ".kimetsu")
            .unwrap_or(false)
        {
            continue;
        }
        candidates.push((mtime, rel));
    }
    candidates.sort_by_key(|b| std::cmp::Reverse(b.0));
    candidates
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

fn run_git(workspace: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                use std::io::Read;
                let mut buf = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    stdout.read_to_string(&mut buf).ok()?;
                }
                return Some(buf);
            }
            Ok(Some(_)) => return None,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_omits_empty_fields() {
        let ctx = AmbientContext::default();
        assert_eq!(render_as_query_suffix(&ctx), "");
        assert_eq!(augment_query("plan the patch", &ctx), "plan the patch");
    }

    #[test]
    fn render_includes_branch_only_when_present() {
        let ctx = AmbientContext {
            branch: Some("feature/embedder".to_string()),
            ..Default::default()
        };
        let suffix = render_as_query_suffix(&ctx);
        assert!(suffix.contains("branch=feature/embedder"), "got {suffix}");
        assert!(!suffix.contains("recent:"));
        assert!(!suffix.contains("dirty:"));
    }

    #[test]
    fn render_normalizes_windows_path_separators() {
        let ctx = AmbientContext {
            recent_files: vec![PathBuf::from("src\\embeddings.rs")],
            git_status: vec![StatusEntry {
                flag: " M".into(),
                path: PathBuf::from("crates\\kimetsu-brain\\src\\ambient.rs"),
            }],
            ..Default::default()
        };
        let suffix = render_as_query_suffix(&ctx);
        assert!(suffix.contains("src/embeddings.rs"), "got {suffix}");
        assert!(
            suffix.contains("crates/kimetsu-brain/src/ambient.rs"),
            "got {suffix}"
        );
        assert!(!suffix.contains('\\'), "backslashes must be normalized");
    }

    #[test]
    fn render_collapses_multiple_fields_with_separator() {
        let ctx = AmbientContext {
            branch: Some("main".into()),
            git_status: vec![StatusEntry {
                flag: "??".into(),
                path: PathBuf::from("new.rs"),
            }],
            recent_files: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            collected_at_unix: 0,
        };
        let suffix = render_as_query_suffix(&ctx);
        assert!(suffix.starts_with("\n[workspace:"));
        assert!(suffix.ends_with(']'));
        // All three blocks present, separated by " | ".
        assert!(suffix.contains("branch=main"));
        assert!(suffix.contains("recent: a.rs, b.rs"));
        assert!(suffix.contains("dirty: ?? new.rs"));
        let pipe_count = suffix.matches(" | ").count();
        assert_eq!(pipe_count, 2, "three blocks -> two separators");
    }

    #[test]
    fn augment_query_appends_suffix_when_nonempty() {
        let ctx = AmbientContext {
            branch: Some("dev".into()),
            ..Default::default()
        };
        let out = augment_query("fix it", &ctx);
        assert!(out.starts_with("fix it"));
        assert!(out.contains("branch=dev"));
    }

    #[test]
    fn parse_git_status_handles_typical_lines() {
        let sample = " M src/a.rs\n?? src/b.rs\nMM src/c.rs\nA  src/d.rs\nbadly-formatted\n";
        let parsed = parse_git_status(sample, 10);
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].flag, " M");
        assert_eq!(parsed[0].path, PathBuf::from("src/a.rs"));
        assert_eq!(parsed[1].flag, "??");
        assert_eq!(parsed[2].flag, "MM");
        assert_eq!(parsed[3].flag, "A ");
    }

    #[test]
    fn parse_git_status_respects_limit() {
        let sample =
            " M one\n M two\n M three\n M four\n M five\n M six\n M seven\n M eight\n M nine\n";
        let parsed = parse_git_status(sample, 3);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].path, PathBuf::from("one"));
        assert_eq!(parsed[2].path, PathBuf::from("three"));
    }

    #[test]
    fn ambient_enabled_respects_env() {
        // Acquire the shared brain test env lock so we don't race
        // with the embedder/user-brain env tests.
        let _lock = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_AMBIENT").ok();
        // SAFETY: env mutation is serialized under the brain test
        // env lock for the duration of this test.
        unsafe {
            std::env::remove_var("KIMETSU_BRAIN_AMBIENT");
        }
        assert!(ambient_enabled(), "default ON");
        for off in ["off", "0", "false", "no", "NONE"] {
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_AMBIENT", off);
            }
            assert!(!ambient_enabled(), "value {off:?} should disable");
        }
        for on in ["on", "1", "true", "yes", "anything-else"] {
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_AMBIENT", on);
            }
            assert!(ambient_enabled(), "value {on:?} should enable");
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_AMBIENT", v),
                None => std::env::remove_var("KIMETSU_BRAIN_AMBIENT"),
            }
        }
    }

    #[test]
    fn collect_recent_files_skips_dotkimetsu() {
        // Build a fake workspace with `.kimetsu/` content next to
        // real source files; the walker must surface the source
        // files only.
        let root = std::env::temp_dir().join(format!("kimetsu-ambient-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".kimetsu/runs")).unwrap();
        std::fs::write(root.join("src/a.rs"), "// a").unwrap();
        std::fs::write(root.join("src/b.rs"), "// b").unwrap();
        std::fs::write(root.join(".kimetsu/runs/01.trace"), "noise").unwrap();

        let files = collect_recent_files(&root, 5, Duration::from_secs(2));
        assert!(
            files.iter().all(|p| !p.starts_with(".kimetsu")),
            ".kimetsu/ entries should be filtered out: {:?}",
            files
        );
        assert!(
            files
                .iter()
                .any(|p| p.ends_with("a.rs") || p.ends_with("b.rs"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
