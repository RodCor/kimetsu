use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use kimetsu_core::paths::ProjectPaths;

#[derive(Debug, Clone)]
pub struct RunPaths {
    pub run_dir: PathBuf,
    pub trace_jsonl: PathBuf,
    pub artifacts_dir: PathBuf,
    pub patch_plans_dir: PathBuf,
    pub final_report: PathBuf,
    pub run_log: PathBuf,
}

impl RunPaths {
    pub fn new(paths: &ProjectPaths, run_id: RunId) -> Self {
        let run_dir = paths.runs_dir.join(run_id.to_string());
        Self {
            trace_jsonl: run_dir.join("trace.jsonl"),
            artifacts_dir: run_dir.join("artifacts"),
            patch_plans_dir: run_dir.join("patch_plans"),
            final_report: run_dir.join("final_report.md"),
            run_log: run_dir.join("kimetsu.log"),
            run_dir,
        }
    }

    pub fn create_dirs(&self) -> KimetsuResult<()> {
        fs::create_dir_all(&self.artifacts_dir)?;
        fs::create_dir_all(&self.patch_plans_dir)?;
        Ok(())
    }
}

pub struct TraceWriter {
    file: File,
}

impl TraceWriter {
    pub fn create(paths: &ProjectPaths, run_id: RunId) -> KimetsuResult<(Self, RunPaths)> {
        let run_paths = RunPaths::new(paths, run_id);
        run_paths.create_dirs()?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&run_paths.trace_jsonl)?;

        // Opportunistic GC: prune old sibling run dirs when a new one is
        // created (rare — only real agent runs). Runs only when
        // KIMETSU_RUNS_GC != "0".  Best-effort: never fails the run.
        if std::env::var("KIMETSU_RUNS_GC").as_deref() != Ok("0") {
            gc_old_runs(
                &paths.runs_dir,
                Duration::from_secs(30 * 24 * 3600), // 30 days
                20,                                  // keep newest 20
            );
        }

        Ok((Self { file }, run_paths))
    }

    pub fn append(&mut self, event: &Event, fsync: bool) -> KimetsuResult<()> {
        serde_json::to_writer(&mut self.file, event)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        if fsync {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

pub fn read_trace(trace_jsonl: &Path) -> KimetsuResult<Vec<Event>> {
    let file = File::open(trace_jsonl)?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut line = String::new();
    let mut line_number = 0usize;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        line_number += 1;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<Event>(trimmed) {
            Ok(event) => events.push(event),
            Err(err) => {
                if !line.ends_with('\n') {
                    eprintln!(
                        "warning: ignoring invalid trailing JSONL line in {}: {err}",
                        trace_jsonl.display()
                    );
                    break;
                }

                return Err(format!(
                    "invalid JSONL at {}:{}: {err}",
                    trace_jsonl.display(),
                    line_number
                )
                .into());
            }
        }
    }

    Ok(events)
}

pub fn discover_traces(paths: &ProjectPaths) -> KimetsuResult<Vec<PathBuf>> {
    if !paths.runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut traces = Vec::new();
    for entry in fs::read_dir(&paths.runs_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let trace = entry.path().join("trace.jsonl");
        if trace.exists() {
            traces.push(trace);
        }
    }

    traces.sort();
    Ok(traces)
}

pub fn read_all_traces(paths: &ProjectPaths) -> KimetsuResult<Vec<Event>> {
    let mut events = Vec::new();
    for trace in discover_traces(paths)? {
        events.extend(read_trace(&trace)?);
    }

    events.sort_by(|left, right| {
        left.event_id
            .0
            .cmp(&right.event_id.0)
            .then_with(|| left.ts.cmp(&right.ts))
    });
    events.dedup_by_key(|event| event.event_id);
    Ok(events)
}

// ---------------------------------------------------------------------------
// Auto-GC: opportunistic pruning of old run dirs on new-run creation
// ---------------------------------------------------------------------------

/// Extract the run-start timestamp (Unix ms) from a ULID directory name.
/// Returns `None` when the string is not a valid ULID.
fn ulid_timestamp_ms(name: &str) -> Option<u64> {
    name.parse::<ulid::Ulid>().ok().map(|u| u.timestamp_ms())
}

/// Pure selection function for auto-GC.
///
/// Given a slice of `(name, ts_ms)` run entries (the caller must sort them
/// newest-first before calling), returns the indices of entries that should
/// be removed according to the policy:
///
/// * The newest `keep` entries are always protected (indices `0..keep`).
/// * Beyond that, entries whose `ts_ms` is older than `now_ms - max_age`
///   are selected for removal.
/// * Empty input → empty output.
pub fn select_old_runs(
    runs: &[(&str, u64)],
    now_ms: u64,
    max_age: Duration,
    keep: usize,
) -> Vec<usize> {
    let cutoff_ms = now_ms.saturating_sub(max_age.as_millis() as u64);
    runs.iter()
        .enumerate()
        .filter_map(|(idx, (_name, ts_ms))| {
            if idx < keep {
                return None; // always protected
            }
            if *ts_ms < cutoff_ms { Some(idx) } else { None }
        })
        .collect()
}

/// Opportunistic GC: scan `runs_dir` for ULID-named subdirs, remove those
/// older than `max_age` while always keeping the `keep` newest.
///
/// Best-effort: per-directory errors are swallowed and never propagate to
/// the caller. The function is a no-op when `runs_dir` doesn't exist.
pub fn gc_old_runs(runs_dir: &Path, max_age: Duration, keep: usize) {
    let Ok(rd) = fs::read_dir(runs_dir) else {
        return;
    };

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Collect (name, ts_ms, path) for each subdirectory.
    let mut entries: Vec<(String, u64, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let ts_ms = ulid_timestamp_ms(&name).unwrap_or_else(|| {
                e.metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            });
            (name, ts_ms, path)
        })
        .collect();

    // Sort newest-first so the protection guard is correct.
    entries.sort_by_key(|(_, ts, _)| std::cmp::Reverse(*ts));

    // Build the slim (name, ts_ms) slice for the pure selection fn.
    let slim: Vec<(&str, u64)> = entries
        .iter()
        .map(|(name, ts, _)| (name.as_str(), *ts))
        .collect();

    let to_remove = select_old_runs(&slim, now_ms, max_age, keep);
    for idx in to_remove {
        let _ = fs::remove_dir_all(&entries[idx].2);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kimetsu_core::ids::RunId;
    use kimetsu_core::paths::ProjectPaths;
    use std::sync::Mutex;

    /// Process-wide mutex so env-mutating tests don't race.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: Mutex<()> = Mutex::new(());
        &LOCK
    }

    // ── select_old_runs: pure unit tests ────────────────────────────────────

    #[test]
    fn select_empty_returns_empty() {
        let result = select_old_runs(&[], 1_000_000_000, Duration::from_secs(1), 0);
        assert!(result.is_empty());
    }

    #[test]
    fn select_newer_than_cutoff_not_selected() {
        let now_ms: u64 = 1_000_000_000;
        let max_age = Duration::from_secs(3 * 24 * 3600); // 3 days
        let cutoff_ms = now_ms - max_age.as_millis() as u64;
        // All runs are newer than the cutoff.
        let runs = vec![
            ("run-1", cutoff_ms + 10_000),
            ("run-2", cutoff_ms + 5_000),
            ("run-3", cutoff_ms + 1_000),
        ];
        let result = select_old_runs(&runs, now_ms, max_age, 0);
        assert!(
            result.is_empty(),
            "runs newer than cutoff must not be selected"
        );
    }

    #[test]
    fn select_older_than_cutoff_selected() {
        let now_ms: u64 = 1_000_000_000;
        let max_age = Duration::from_secs(3 * 24 * 3600); // 3 days
        let cutoff_ms = now_ms - max_age.as_millis() as u64;
        // All runs older than the cutoff, no keep protection.
        let runs = vec![("run-1", cutoff_ms - 1_000), ("run-2", cutoff_ms - 5_000)];
        let result = select_old_runs(&runs, now_ms, max_age, 0);
        assert_eq!(result, vec![0, 1], "both old runs should be selected");
    }

    #[test]
    fn select_keep_protects_newest() {
        let now_ms: u64 = 1_000_000_000;
        // A large max_age so everything qualifies on age.
        let max_age = Duration::from_secs(1);
        // Slice already sorted newest-first.
        let runs = vec![
            ("run-a", 900),
            ("run-b", 800),
            ("run-c", 700),
            ("run-d", 600),
        ];
        // Keep 2 → protect indices 0 and 1.
        let result = select_old_runs(&runs, now_ms, max_age, 2);
        assert_eq!(result, vec![2, 3]);
    }

    #[test]
    fn select_keep_larger_than_slice_selects_nothing() {
        let now_ms: u64 = 1_000_000_000;
        let max_age = Duration::from_secs(1);
        let runs = vec![("run-1", 100), ("run-2", 50)];
        let result = select_old_runs(&runs, now_ms, max_age, 10);
        assert!(
            result.is_empty(),
            "keep >= slice length must protect everything"
        );
    }

    #[test]
    fn select_mixed_age_and_keep() {
        // now = 10 days in ms, max_age = 2 days.
        // runs sorted newest-first:
        //   idx 0: 9-day-old  → protected by keep=2
        //   idx 1: 8-day-old  → protected by keep=2
        //   idx 2: 5-day-old  → older than 2d but inside keep? No, idx=2 ≥ keep=2
        //   idx 3: 1-day-old  → newer than cutoff (1d < 2d)
        //   idx 4: 3-day-old  → older than 2d, idx=4 ≥ keep=2 → selected
        let day_ms = 24u64 * 3600 * 1_000;
        let now_ms = 10 * day_ms;
        let max_age = Duration::from_secs(2 * 24 * 3600);
        let runs = vec![
            ("r0", now_ms - 9 * day_ms),
            ("r1", now_ms - 8 * day_ms),
            ("r2", now_ms - 5 * day_ms),
            ("r3", now_ms - day_ms),
            ("r4", now_ms - 3 * day_ms),
        ];
        // keep=2 → protect r0, r1
        // cutoff = now - 2d → r3 (1d old) is newer, not selected
        //                    r2 (5d old), r4 (3d old) → both older, selected
        let result = select_old_runs(&runs, now_ms, max_age, 2);
        assert_eq!(result, vec![2, 4]);
    }

    // ── gc_old_runs: filesystem tests ───────────────────────────────────────

    /// Create a temp runs dir with N fake run subdirs.
    /// `age_ms_offsets` is the list of (dir-suffix, ms-before-now).
    /// Uses real ULID-like names with a fake timestamp encoded by creating
    /// the dirs but naming them with synthetic names + storing age via
    /// mtime manipulation (not feasible portably) — instead we test using
    /// real ULID dirs where we can control timestamps by calling
    /// gc_old_runs with an adjusted `now_ms` analog.
    ///
    /// Since gc_old_runs computes its own now_ms internally, we test
    /// indirectly via the pure function + a filesystem integration test
    /// that checks removal correctness by making ALL dirs "very old" or
    /// "very new" via ULID timestamp manipulation — which we can't do
    /// after the fact.
    ///
    /// Instead: we test gc_old_runs via non-ULID dirs whose mtime IS the
    /// encoded age.  We use a large max_age (e.g., 365 days) so only
    /// truly ancient dirs are removed; all freshly-created dirs survive.
    #[test]
    fn gc_fresh_dirs_all_survive() {
        let tmp = std::env::temp_dir().join(format!("kimetsu-gc-fresh-{}", RunId::new()));
        fs::create_dir_all(&tmp).unwrap();

        // Create 5 fresh "run" subdirs.
        for i in 0..5u32 {
            fs::create_dir_all(tmp.join(format!("run-{i}"))).unwrap();
        }

        // max_age = 30 days; fresh dirs are <1 second old → all survive.
        gc_old_runs(&tmp, Duration::from_secs(30 * 24 * 3600), 20);

        let remaining: Vec<_> = fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        assert_eq!(remaining.len(), 5, "all 5 fresh dirs should survive");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn gc_env_zero_disables_gc() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let tmp = std::env::temp_dir().join(format!("kimetsu-gc-env0-{}", RunId::new()));
        fs::create_dir_all(&tmp).unwrap();

        for i in 0..3u32 {
            fs::create_dir_all(tmp.join(format!("run-{i}"))).unwrap();
        }

        // With KIMETSU_RUNS_GC=0 the TraceWriter::create branch skips GC.
        // We can test the env skip by asserting that gc_old_runs is NOT
        // called — but the easiest check is the opt-out in TraceWriter::create.
        // Here we test that even if gc_old_runs is called with an absurdly
        // small max_age + keep=0, the env guard in TraceWriter is the wall.
        // We test TraceWriter integration below; here we just confirm that
        // gc_old_runs itself with keep=3 protects all 3 runs.
        gc_old_runs(&tmp, Duration::from_nanos(1), 3); // keep=3 protects all
        let remaining: usize = fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .count();
        assert_eq!(remaining, 3, "keep=3 should protect all 3 dirs");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn trace_writer_create_env_zero_skips_gc() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());

        let root = std::env::temp_dir().join(format!("kimetsu-gc-tw-{}", RunId::new()));
        fs::create_dir_all(&root).unwrap();
        kimetsu_core::paths::git_init_boundary(&root);
        kimetsu_brain_init_for_test(&root);

        let paths = ProjectPaths::at_root(&root);

        // Create a "sibling" run dir.
        fs::create_dir_all(paths.runs_dir.join("old-sibling")).unwrap();

        unsafe { std::env::set_var("KIMETSU_RUNS_GC", "0") };
        let run_id = RunId::new();
        let (_tw, run_paths) = TraceWriter::create(&paths, run_id).expect("create");
        // With GC=0, the sibling must NOT be removed.
        assert!(
            paths.runs_dir.join("old-sibling").exists(),
            "GC=0 must leave old-sibling untouched"
        );
        // The just-created run must exist.
        assert!(run_paths.run_dir.exists(), "new run dir must exist");
        unsafe { std::env::remove_var("KIMETSU_RUNS_GC") };

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn trace_writer_create_new_run_survives_gc() {
        let _guard = env_lock().lock().unwrap_or_else(|p| p.into_inner());

        let root = std::env::temp_dir().join(format!("kimetsu-gc-survive-{}", RunId::new()));
        fs::create_dir_all(&root).unwrap();
        kimetsu_core::paths::git_init_boundary(&root);
        kimetsu_brain_init_for_test(&root);

        let paths = ProjectPaths::at_root(&root);

        // Ensure GC is enabled.
        unsafe { std::env::remove_var("KIMETSU_RUNS_GC") };

        let run_id = RunId::new();
        let (_tw, run_paths) = TraceWriter::create(&paths, run_id).expect("create");

        // The newly-created run dir must always survive (it's the newest).
        assert!(
            run_paths.run_dir.exists(),
            "just-created run dir must survive GC"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // Helper: initialize only the runs_dir (no full project.toml / brain.db needed
    // for trace tests).
    fn kimetsu_brain_init_for_test(root: &Path) {
        let kimetsu_dir = root.join(".kimetsu");
        fs::create_dir_all(kimetsu_dir.join("runs")).unwrap();
    }
}
