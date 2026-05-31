//! v0.8: per-session state for the proactive recall hooks.
//!
//! The proactive PreToolUse/PostToolUse hooks run as fresh CLI
//! processes per tool call (the "stateless now, daemon later" model),
//! so cross-call memory lives in a tiny JSON file under
//! `<repo>/.kimetsu/proactive/<session_id>.json`. It powers three
//! brain-like behaviours:
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

/// Best-effort save — failures are swallowed (a hook must never break
/// the agent's turn just because it couldn't persist its own state).
pub fn save(path: &Path, state: &SessionState) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(state) {
        let _ = fs::write(path, text);
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

/// Derive a short, stable signature from failing tool output: the
/// first line that looks like an error, truncated. Used to tell apart
/// "same command, same failure" (a real loop) from incidental reruns.
pub fn error_signature(tool_response: &str) -> Option<String> {
    let line = tool_response
        .lines()
        .find(|l| {
            let lc = l.to_ascii_lowercase();
            FAILURE_MARKERS.iter().any(|m| lc.contains(m))
        })
        .or_else(|| tool_response.lines().find(|l| !l.trim().is_empty()))?;
    let sig: String = line.trim().chars().take(80).collect();
    if sig.is_empty() { None } else { Some(sig) }
}

/// Substrings that mark tool output as a failure. Shared by
/// [`error_signature`] and the hook's failure gate.
pub const FAILURE_MARKERS: &[&str] = &[
    "error",
    "failed",
    "fatal",
    "panic",
    "exception",
    "traceback",
    "denied",
    "not found",
    "cannot",
    "no such",
];

/// True when tool output looks like a failure (case-insensitive).
pub fn looks_like_failure(tool_response: &str) -> bool {
    let lc = tool_response.to_ascii_lowercase();
    FAILURE_MARKERS.iter().any(|m| lc.contains(m))
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
    fn error_signature_picks_error_line() {
        let sig =
            error_signature("compiling...\nerror: linker `link.exe` not found\nmore").unwrap();
        assert!(sig.to_ascii_lowercase().contains("linker"));
        assert!(error_signature("").is_none());
    }
}
