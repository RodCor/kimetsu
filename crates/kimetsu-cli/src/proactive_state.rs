//! v0.8: per-session state for the proactive recall hooks.
//!
//! The proactive PreToolUse/PostToolUse hooks run as fresh CLI
//! processes per tool call (the "stateless now, daemon later" model),
//! so cross-call memory lives in a tiny JSON file under the per-project
//! user cache, `~/.kimetsu/cache/<project-hash>/proactive/<session_id>.json`
//! (kept out of the project's `.kimetsu/` so the brain folder stays lean).
//! It powers three brain-like behaviours:
//!
//!   * **Dedupe** — a memory surfaces at most once per session
//!     (`surfaced_memory_ids`). Once it's "in working memory" it
//!     doesn't keep re-announcing itself.
//!   * **Refractory throttle** — after an injection we stay quiet for
//!     a short window (`last_injection_unix`) so the agent isn't
//!     flooded with associations.
//!   * **Loop detection** — repeated (command, error) pairs bump a
//!     counter (`recent_commands`); once it crosses [`LOOP_THRESHOLD`]
//!     the hook drops to a lower score floor and bypasses the
//!     refractory once, because a stuck agent should get help sooner.
//!
//! Every read/write is best-effort: a corrupt or missing file just
//! resets to default. The file is intentionally small (a ring buffer
//! of recent commands plus a handful of ids), and stale session files
//! are garbage-collected opportunistically on each invocation.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Repeated (command, error) observations before loop mode kicks in.
pub const LOOP_THRESHOLD: u32 = 3;

/// v0.8.5: minimum gap between `[kimetsu-harvest]` cues in one session.
/// A single fix shouldn't fire repeatedly, and the end-of-session cue
/// shares this throttle so the agent is nudged at most a couple of times.
pub const HARVEST_REFRACTORY_SECS: u64 = 600;

const MAX_RECENT_COMMANDS: usize = 12;
const SESSION_GC_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(default)]
    pub surfaced_memory_ids: Vec<String>,
    #[serde(default)]
    pub last_injection_unix: u64,
    #[serde(default)]
    pub recent_commands: Vec<CmdSeen>,
    /// v0.8.5: unix time of the last `[kimetsu-harvest]` cue this
    /// session. Throttles how often we nudge the agent to dispatch the
    /// memory-harvester subagent (a failed-then-fixed command, or a
    /// non-trivial session that recorded nothing). `0` = never cued.
    #[serde(default)]
    pub harvest_cued_unix: u64,
    /// v2.6: unix time this session was handed the warm-start block (repo
    /// digest + episodic resume) by the per-turn context hook. Hosts with
    /// no session-start event get it prepended to their first prompt
    /// instead; this flag keeps it to exactly one turn. `0` = not yet warmed.
    #[serde(default)]
    pub warm_started_unix: u64,
    /// v2.6: how many proactive injections this session has already made.
    /// Feeds the injection policy's novelty feature — the tenth interruption
    /// is worth less than the first.
    #[serde(default)]
    pub injections: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdSeen {
    /// Normalized command text (lowercased, whitespace-collapsed).
    pub norm: String,
    /// Short signature of the failure, when this came from a failed
    /// PostToolUse. `None` for pre-command observations.
    #[serde(default)]
    pub error_sig: Option<String>,
    pub count: u32,
    pub last_unix: u64,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the on-disk path for a session's state, sanitizing the
/// session id so it can't escape the `proactive/` directory.
pub fn session_path(kimetsu_dir: &Path, session_id: Option<&str>) -> PathBuf {
    let raw = session_id.map(str::trim).filter(|s| !s.is_empty());
    let safe = match raw {
        Some(id) => sanitize_id(id),
        None => "_no_session".to_string(),
    };
    kimetsu_dir.join("proactive").join(format!("{safe}.json"))
}

fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    if cleaned.is_empty() {
        "_no_session".to_string()
    } else {
        cleaned
    }
}

/// Best-effort load — any error (missing/corrupt) yields a fresh state.
pub fn load(path: &Path) -> SessionState {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic write: serialise `state` to a sibling `.tmp` file, then rename
/// it over `path`.  rename(2) is atomic on the same filesystem so a reader
/// always sees either the previous complete file or the new complete file —
/// never a torn partial write.  All failures are swallowed; a hook must
/// never break the agent's turn because of a state-persistence hiccup.
pub fn save(path: &Path, state: &SessionState) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(parent);
    let Ok(text) = serde_json::to_string(state) else {
        return;
    };
    // Same dir → same filesystem, so the rename below is always atomic.
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, &text).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Delete session files older than the GC horizon. Best-effort and
/// cheap (the directory holds at most a handful of tiny files).
pub fn gc(kimetsu_dir: &Path, now: u64) {
    let dir = kimetsu_dir.join("proactive");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let age_ok = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_secs()) > SESSION_GC_MAX_AGE_SECS)
            .unwrap_or(false);
        if age_ok {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Collapse a command to a stable comparison key: lowercase + single
/// spaces. Cheap and good enough to catch a literally-repeated command.
pub fn normalize_command(cmd: &str) -> String {
    cmd.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

// v2.6: failure detection moved to `crate::tool_outcome`.
//
// It used to live here as a case-insensitive substring scan for ten words,
// which fired on every passing test suite ("0 failed" contains "failed") and
// on every crate named `error-chain`. The replacement reads the exit code when
// the harness reports one, parses the toolchain summary line when it can, and
// only falls back to the substring scan when there is nothing better.

// -----------------------------------------------------------------------
// v1.5 (Story 2.3): session-scoped cross-turn capsule dedupe
// -----------------------------------------------------------------------

/// Pure dedupe-decision function. Given a slice of capsules and the current
/// session state, returns the indices of capsules that should be injected:
///
///   * Capsules whose `expansion_handle` is already surfaced this session
///     are skipped.
///   * **Soft policy**: if the filter would leave zero capsules (every
///     candidate is already surfaced), the full input slice is returned
///     — a repeated top memory may still be the right context.
///   * Capsules with an empty `expansion_handle` are never tracked and
///     always pass through.
///
/// Returns a `Vec<usize>` of indices into `handles` that survive the filter.
/// Callers iterate these to collect the actual capsule objects and later
/// call `state.mark_surfaced` on the included handles.
///
/// This is a pure function of (handles, state) with no I/O — easy to unit
/// test without touching the file system.
pub fn dedupe_filter(handles: &[&str], state: &SessionState) -> Vec<usize> {
    let new_indices: Vec<usize> = handles
        .iter()
        .enumerate()
        .filter(|(_, h)| h.is_empty() || !state.is_surfaced(h))
        .map(|(i, _)| i)
        .collect();

    // Soft policy: never empty the injection.
    if new_indices.is_empty() {
        (0..handles.len()).collect()
    } else {
        new_indices
    }
}

impl SessionState {
    pub fn is_surfaced(&self, memory_id: &str) -> bool {
        self.surfaced_memory_ids.iter().any(|id| id == memory_id)
    }

    pub fn mark_surfaced(&mut self, memory_id: &str) {
        if !self.is_surfaced(memory_id) {
            self.surfaced_memory_ids.push(memory_id.to_string());
        }
    }

    /// True when a prior injection happened within `secs` of `now`.
    pub fn in_refractory(&self, now: u64, secs: u64) -> bool {
        self.last_injection_unix > 0 && now.saturating_sub(self.last_injection_unix) < secs
    }

    pub fn record_injection(&mut self, now: u64) {
        self.last_injection_unix = now;
        self.injections = self.injections.saturating_add(1);
    }

    /// v2.6: how many proactive injections this session has already made.
    pub fn injection_count(&self) -> u32 {
        self.injections
    }

    /// v2.6: how far past the refractory window we are, in `[0, 1]`.
    ///
    /// 0 immediately after an injection, 1 once well clear of it. A feature
    /// rather than a gate: the refractory check is still a hard rule, but the
    /// policy can learn that an injection *just* past the window is worth less
    /// than one after a long quiet stretch.
    pub fn recovery_fraction(&self, now: u64, refractory_secs: u64) -> f32 {
        if self.last_injection_unix == 0 || refractory_secs == 0 {
            return 1.0;
        }
        let elapsed = now.saturating_sub(self.last_injection_unix) as f32;
        (elapsed / refractory_secs as f32).clamp(0.0, 1.0)
    }

    /// v0.8.5: True when `norm` was seen failing earlier this session
    /// (any tracked observation of it carries an error signature). Used
    /// by the PostToolUse hook to detect "a previously-failing command
    /// just succeeded" — a high-signal learnable moment.
    pub fn had_prior_failure(&self, norm: &str) -> bool {
        self.recent_commands
            .iter()
            .any(|c| c.norm == norm && c.error_sig.is_some())
    }

    /// v0.8.5: Drop the failure observations for `norm` (call after a
    /// resolution cue so the same fix isn't re-harvested if the command
    /// is run again).
    pub fn clear_failure(&mut self, norm: &str) {
        self.recent_commands
            .retain(|c| !(c.norm == norm && c.error_sig.is_some()));
    }

    /// v0.8.5: True when a harvest cue fired within `secs` of `now`.
    pub fn harvest_in_refractory(&self, now: u64, secs: u64) -> bool {
        self.harvest_cued_unix > 0 && now.saturating_sub(self.harvest_cued_unix) < secs
    }

    /// v0.8.5: True when a harvest cue has fired at all this session.
    /// The end-of-session (Stop) cue uses this to fire at most once.
    pub fn harvest_cued(&self) -> bool {
        self.harvest_cued_unix > 0
    }

    pub fn note_harvest_cue(&mut self, now: u64) {
        self.harvest_cued_unix = now;
    }

    /// Record an observation of `norm`/`error_sig` and return the new
    /// running count for that pair. Keeps a bounded ring buffer so the
    /// state file stays tiny.
    pub fn note_command(&mut self, norm: &str, error_sig: Option<&str>, now: u64) -> u32 {
        if let Some(existing) = self
            .recent_commands
            .iter_mut()
            .find(|c| c.norm == norm && c.error_sig.as_deref() == error_sig)
        {
            existing.count = existing.count.saturating_add(1);
            existing.last_unix = now;
            return existing.count;
        }
        self.recent_commands.push(CmdSeen {
            norm: norm.to_string(),
            error_sig: error_sig.map(str::to_string),
            count: 1,
            last_unix: now,
        });
        if self.recent_commands.len() > MAX_RECENT_COMMANDS {
            // Drop the oldest by last_unix.
            if let Some((idx, _)) = self
                .recent_commands
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_unix)
            {
                self.recent_commands.remove(idx);
            }
        }
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W2: verify that proactive state saved via `session_path` + the
    /// cache base lands under the cache dir, NOT under any `.kimetsu/`
    /// inside the repo.
    #[test]
    fn proactive_state_lands_in_cache_not_in_kimetsu() {
        let tmp = std::env::temp_dir();
        // Simulate a cache dir that looks like ~/.kimetsu/cache/<id>.
        let cache_dir = tmp.join("kimetsu-w2-test-cache").join("my-repo");
        // A fake repo root — its .kimetsu must NOT receive any file.
        let repo_root = tmp.join("kimetsu-w2-test-repo");
        let kimetsu_dir = repo_root.join(".kimetsu");

        let p = session_path(&cache_dir, Some("test-session"));
        // State lives under the cache dir, in a `proactive/` subdirectory.
        assert!(
            p.starts_with(&cache_dir),
            "state path {p:?} must be under cache_dir {cache_dir:?}"
        );
        assert!(
            p.to_string_lossy().contains("proactive"),
            "state path must contain 'proactive', got {p:?}"
        );
        // It must NOT be inside the repo's .kimetsu.
        assert!(
            !p.starts_with(&kimetsu_dir),
            "state path must not be inside .kimetsu"
        );

        // Round-trip: save + load via the cache path.
        let mut state = SessionState::default();
        state.mark_surfaced("mem-123");
        save(&p, &state);
        let loaded = load(&p);
        assert!(
            loaded.is_surfaced("mem-123"),
            "loaded state must match saved"
        );

        // .kimetsu/proactive must NOT have been created.
        assert!(
            !kimetsu_dir.join("proactive").exists(),
            ".kimetsu/proactive must not be created"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&repo_root);
    }

    #[test]
    fn dedupe_marks_once() {
        let mut s = SessionState::default();
        assert!(!s.is_surfaced("m1"));
        s.mark_surfaced("m1");
        s.mark_surfaced("m1");
        assert!(s.is_surfaced("m1"));
        assert_eq!(s.surfaced_memory_ids.len(), 1);
    }

    #[test]
    fn refractory_window() {
        let mut s = SessionState::default();
        assert!(!s.in_refractory(100, 90));
        s.record_injection(100);
        assert!(s.in_refractory(150, 90));
        assert!(!s.in_refractory(200, 90));
    }

    #[test]
    fn loop_counter_reaches_threshold() {
        let mut s = SessionState::default();
        let norm = normalize_command("cargo  BUILD");
        assert_eq!(norm, "cargo build");
        assert_eq!(s.note_command(&norm, Some("error: linker"), 1), 1);
        assert_eq!(s.note_command(&norm, Some("error: linker"), 2), 2);
        let third = s.note_command(&norm, Some("error: linker"), 3);
        assert_eq!(third, LOOP_THRESHOLD);
        // A different error signature tracks separately.
        assert_eq!(s.note_command(&norm, Some("other failure"), 4), 1);
    }

    #[test]
    fn ring_buffer_is_bounded() {
        let mut s = SessionState::default();
        for i in 0..(MAX_RECENT_COMMANDS + 5) {
            s.note_command(&format!("cmd {i}"), None, i as u64);
        }
        assert!(s.recent_commands.len() <= MAX_RECENT_COMMANDS);
    }

    #[test]
    fn session_path_sanitizes() {
        let dir = Path::new("/tmp/.kimetsu");
        let p = session_path(dir, Some("../../etc/passwd"));
        assert!(p.starts_with("/tmp/.kimetsu/proactive"));
        assert!(!p.to_string_lossy().contains(".."));
        let none = session_path(dir, None);
        assert!(none.to_string_lossy().contains("_no_session"));
    }

    #[test]
    fn failure_then_resolution_is_detectable() {
        let mut s = SessionState::default();
        let norm = normalize_command("cargo build");
        assert!(!s.had_prior_failure(&norm));
        // A failing observation carries an error signature.
        s.note_command(&norm, Some("error: linker"), 1);
        assert!(s.had_prior_failure(&norm));
        // After we cue a harvest for the resolution, clearing the
        // failure stops it from re-firing on a later success.
        s.clear_failure(&norm);
        assert!(!s.had_prior_failure(&norm));
        // A clean (no-error) observation never marks a failure.
        s.note_command(&norm, None, 2);
        assert!(!s.had_prior_failure(&norm));
    }

    #[test]
    fn harvest_cue_throttle() {
        let mut s = SessionState::default();
        assert!(!s.harvest_cued());
        assert!(!s.harvest_in_refractory(100, HARVEST_REFRACTORY_SECS));
        s.note_harvest_cue(100);
        assert!(s.harvest_cued());
        assert!(s.harvest_in_refractory(150, HARVEST_REFRACTORY_SECS));
        assert!(!s.harvest_in_refractory(100 + HARVEST_REFRACTORY_SECS, HARVEST_REFRACTORY_SECS));
    }

    /// Kept here (rather than moved wholesale) because it is the behaviour the
    /// loop detector depends on: the same command failing the same way twice
    /// has to produce the same signature.
    #[test]
    fn error_signature_picks_the_error_line() {
        use crate::tool_outcome::classify;
        let sig = classify(
            "compiling...\nerror: linker `link.exe` not found\nmore",
            None,
        )
        .signature
        .expect("a failing build must yield a signature");
        assert!(sig.to_ascii_lowercase().contains("linker"));
        assert!(classify("", None).signature.is_none());
    }

    // ── v1.5 Story 2.3: dedupe_filter unit tests ─────────────────────────────

    /// DD-1: a handle not in the session state passes through.
    #[test]
    fn dedupe_filter_passes_unsurfaced_handles() {
        let state = SessionState::default();
        let handles = ["memory:aaa", "memory:bbb"];
        let indices = dedupe_filter(&handles, &state);
        assert_eq!(indices, vec![0, 1], "unsurfaced capsules must all pass");
    }

    /// DD-2: a handle already surfaced this session is filtered out.
    #[test]
    fn dedupe_filter_removes_surfaced_handles() {
        let mut state = SessionState::default();
        state.mark_surfaced("memory:aaa");

        let handles = ["memory:aaa", "memory:bbb"];
        let indices = dedupe_filter(&handles, &state);
        // Only index 1 ("memory:bbb") should pass.
        assert_eq!(indices, vec![1], "surfaced capsule must be filtered");
    }

    /// DD-3: soft policy — if ALL handles are already surfaced, the full
    /// set is returned (never empty the injection).
    #[test]
    fn dedupe_filter_never_empties_injection() {
        let mut state = SessionState::default();
        state.mark_surfaced("memory:aaa");
        state.mark_surfaced("memory:bbb");

        let handles = ["memory:aaa", "memory:bbb"];
        let indices = dedupe_filter(&handles, &state);
        // Soft policy: return all indices rather than an empty vec.
        assert_eq!(indices, vec![0, 1], "must not empty the injection set");
    }

    /// DD-4: empty expansion_handle is never tracked and always passes through.
    #[test]
    fn dedupe_filter_passes_empty_handle_always() {
        let mut state = SessionState::default();
        // Even if we tried to surface an empty string, it should always pass.
        state.mark_surfaced("");

        let handles = ["", "memory:real"];
        let indices = dedupe_filter(&handles, &state);
        // Both pass: "" is always a pass-through, "memory:real" is new.
        assert!(indices.contains(&0), "empty handle must always pass");
        assert!(indices.contains(&1), "new real handle must pass");
    }

    /// DD-5: mix of surfaced and new — only new handles pass,
    /// provided at least one is new (soft-policy not triggered).
    #[test]
    fn dedupe_filter_mixed_keeps_new_only() {
        let mut state = SessionState::default();
        state.mark_surfaced("memory:old1");
        state.mark_surfaced("memory:old2");

        let handles = ["memory:old1", "memory:new", "memory:old2"];
        let indices = dedupe_filter(&handles, &state);
        assert_eq!(indices, vec![1], "only the new handle must pass");
    }

    /// S4.3: `save` must use an atomic temp+rename so the reader never sees a
    /// torn file.  After a successful save the target file must exist and be
    /// loadable, and the sibling `.tmp` file must NOT remain on disk.
    #[test]
    fn save_is_atomic_no_tmp_leftover() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "kimetsu-atomic-ps-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let path = session_path(&tmp_dir, Some("atomic-test"));

        let mut state = SessionState::default();
        state.mark_surfaced("mem-atomic");
        save(&path, &state);

        // The target file must exist and be readable.
        let loaded = load(&path);
        assert!(
            loaded.is_surfaced("mem-atomic"),
            "saved state must be loadable"
        );

        // The sibling .tmp file must have been cleaned up by the rename.
        let tmp_path = path.with_extension("tmp");
        assert!(
            !tmp_path.exists(),
            "sibling .tmp file must not remain after atomic save"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// DD-6: proactive-state roundtrip — save surfaced handles, reload,
    /// and verify dedupe_filter still sees them as surfaced.
    #[test]
    fn dedupe_filter_survives_state_roundtrip() {
        let tmp = std::env::temp_dir();
        let state_dir = tmp.join(format!(
            "kimetsu-dedupe-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let path = session_path(&state_dir, Some("test-session-dedupe"));

        let mut state = SessionState::default();
        state.mark_surfaced("memory:persisted");
        save(&path, &state);

        let reloaded = load(&path);
        let handles = ["memory:persisted", "memory:fresh"];
        let indices = dedupe_filter(&handles, &reloaded);
        assert_eq!(
            indices,
            vec![1],
            "persisted handle must still be deduplicated after reload"
        );

        let _ = std::fs::remove_dir_all(&state_dir);
    }
}
