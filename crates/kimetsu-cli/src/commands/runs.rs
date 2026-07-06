//! run listing, pruning, locks.
//! Split out of main.rs (v2.5.1); implementations only — the clap
//! surface stays in main.rs.

#![allow(unused_imports)]
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kimetsu_brain::project;
use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};

use crate::*;

// ── runs prune helpers ────────────────────────────────────────────────────

/// Metadata for a single on-disk run directory. Used by the pure selection
/// logic so tests never touch the filesystem.
#[derive(Debug, Clone)]
pub(crate) struct RunDirInfo {
    /// Directory name (the ULID string, or whatever the dir is named).
    pub(crate) name: String,
    /// Full path to the run directory.
    pub(crate) path: PathBuf,
    /// Run-start timestamp in Unix milliseconds.
    /// Derived from the ULID embedded timestamp when the name is a valid
    /// ULID; falls back to the directory's mtime (converted to ms), or 0
    /// when neither is available.
    pub(crate) started_ms: u64,
    /// Total size of all files in the directory (bytes), best-effort.
    pub(crate) size_bytes: u64,
}

pub(crate) fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".to_string());
    }
    // Split the trailing unit char from the numeric prefix.
    let (num_part, unit) = match s.chars().last() {
        Some(c @ ('d' | 'h' | 'm' | 's')) => (&s[..s.len() - c.len_utf8()], c),
        Some(c) => return Err(format!("unknown duration unit '{c}'; use d/h/m/s")),
        None => return Err("empty duration string".to_string()),
    };
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid duration number '{num_part}' in '{s}'"))?;
    let secs = match unit {
        'd' => n * 86_400,
        'h' => n * 3_600,
        'm' => n * 60,
        's' => n,
        _ => unreachable!(),
    };
    Ok(std::time::Duration::from_secs(secs))
}

/// Extract the run-start timestamp (Unix ms) from a ULID string.
/// Returns `None` when the string is not a valid ULID.
pub(crate) fn ulid_timestamp_ms(name: &str) -> Option<u64> {
    name.parse::<ulid::Ulid>().ok().map(|u| u.timestamp_ms())
}

/// Compute the total size in bytes of all files under `dir`, recursively.
/// Best-effort: skips entries that cannot be stat-ed.
pub(crate) fn dir_size_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size_bytes(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Scan `runs_dir` and return one [`RunDirInfo`] per subdirectory.
/// Non-directory entries are skipped.
pub(crate) fn scan_run_dirs(runs_dir: &Path) -> Vec<RunDirInfo> {
    let Ok(rd) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let mut infos: Vec<RunDirInfo> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();

            // Prefer ULID-embedded time; fall back to mtime.
            let started_ms = ulid_timestamp_ms(&name).unwrap_or_else(|| {
                entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            });

            let size_bytes = dir_size_bytes(&path);
            RunDirInfo {
                name,
                path,
                started_ms,
                size_bytes,
            }
        })
        .collect();

    // Sort by started_ms descending (newest first) for stable ordering.
    infos.sort_by_key(|b| std::cmp::Reverse(b.started_ms));
    infos
}

/// Pure selection function: given a slice of [`RunDirInfo`] (sorted
/// newest-first by `started_ms`), return the indices of runs that should
/// be pruned according to the policy.
///
/// # Policy
///
/// * **`older_than` alone**: prune runs whose `started_ms` is older than
///   `now_ms - older_than.as_millis()`. The newest-N guard is absent, so
///   all qualifying runs are selected.
///
/// * **`keep` alone**: prune everything except the `keep` newest runs
///   (i.e. indices `keep..` in the already-sorted-newest-first slice).
///
/// * **both**: prune runs that are *both* older than the cutoff *and*
///   outside the newest-N. Runs in the newest-N are always protected.
///
/// * **neither**: returns an empty `Vec` (the caller must have already
///   rejected this case with an error).
pub(crate) fn select_runs_to_prune(
    runs: &[RunDirInfo],
    now_ms: u64,
    older_than: Option<std::time::Duration>,
    keep: Option<usize>,
) -> Vec<usize> {
    let cutoff_ms: Option<u64> = older_than.map(|d| now_ms.saturating_sub(d.as_millis() as u64));
    let protect_n = keep.unwrap_or(0);

    runs.iter()
        .enumerate()
        .filter_map(|(idx, info)| {
            // The newest-N are always protected.
            if idx < protect_n {
                return None;
            }
            // Apply older-than cutoff when present.
            if let Some(cutoff) = cutoff_ms {
                if info.started_ms >= cutoff {
                    return None; // not old enough
                }
            } else if keep.is_none() {
                // Neither flag — caller should have blocked this; be safe.
                return None;
            }
            Some(idx)
        })
        .collect()
}

/// Format a byte count as a human-readable string (KB / MB / GB).
pub(crate) fn fmt_bytes(n: u64) -> String {
    if n < 1_024 {
        format!("{n} B")
    } else if n < 1_024 * 1_024 {
        format!("{:.1} KB", n as f64 / 1_024.0)
    } else if n < 1_024 * 1_024 * 1_024 {
        format!("{:.1} MB", n as f64 / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2} GB", n as f64 / (1_024.0 * 1_024.0 * 1_024.0))
    }
}

pub(crate) fn runs(command: RunsCommand) -> KimetsuResult<()> {
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
        RunsCommand::Prune(args) => runs_prune(args),
    }
}

pub(crate) fn runs_prune(args: PruneRunsArgs) -> KimetsuResult<()> {
    // Require at least one selection criterion.
    if args.older_than.is_none() && args.keep.is_none() {
        return Err("specify --older-than and/or --keep".into());
    }

    // Parse --older-than duration.
    let older_than_dur: Option<std::time::Duration> = args
        .older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|e| format!("--older-than: {e}"))?;

    // Resolve workspace root.
    let workspace = match args.workspace {
        Some(p) => p,
        None => env::current_dir()?,
    };

    let paths = kimetsu_core::paths::ProjectPaths::discover(&workspace)?;
    let runs_dir = &paths.runs_dir;

    if !runs_dir.exists() {
        println!("no runs to prune");
        return Ok(());
    }

    let infos = scan_run_dirs(runs_dir);
    let total = infos.len();

    // Current time in ms.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let to_prune = select_runs_to_prune(&infos, now_ms, older_than_dur, args.keep);
    let prune_bytes: u64 = to_prune.iter().map(|&i| infos[i].size_bytes).sum();

    if args.apply {
        let mut removed = 0usize;
        let mut freed = 0u64;
        for &idx in &to_prune {
            let info = &infos[idx];
            match std::fs::remove_dir_all(&info.path) {
                Ok(()) => {
                    removed += 1;
                    freed += info.size_bytes;
                    println!("removed {}", info.name);
                }
                Err(e) => {
                    eprintln!("warning: could not remove {} — {e}", info.name);
                }
            }
        }
        println!("removed {removed} run(s), freed {}", fmt_bytes(freed));
    } else {
        // Dry-run: list what would be removed.
        for &idx in &to_prune {
            println!(
                "would remove {} ({})",
                infos[idx].name,
                fmt_bytes(infos[idx].size_bytes)
            );
        }
        println!(
            "{total} run(s), {} old → would remove {} ({} bytes freed)",
            to_prune.len(),
            to_prune.len(),
            fmt_bytes(prune_bytes)
        );
    }

    Ok(())
}

pub(crate) fn lock(command: LockCommand) -> KimetsuResult<()> {
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

// ─── kimetsu brain eval ───────────────────────────────────────────────────────

pub(crate) fn run_command(command: RunCommand) -> KimetsuResult<()> {
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
        RunCommand::Abort { run_id } => {
            project::abort_run(&env::current_dir()?, &run_id)?;
            println!("run aborted: {run_id}");
            Ok(())
        }
    }
}
