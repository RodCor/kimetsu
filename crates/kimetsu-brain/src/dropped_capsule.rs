//! v1.5: dropped-capsule sidecar for the `retrieval.regret` signal.
//!
//! When the `UserPromptSubmit` hook (`brain_context_hook` in the CLI)
//! runs retrieval, some memory capsules score above zero but are EXCLUDED
//! by the relevance floor — they were "dropped". If a dropped capsule's
//! memory is later *cited* by the model, that is a **regret**: the floor
//! was too aggressive and we missed useful context. That error signal
//! feeds the Self-Tuning Brain (v1.5 ROI ledger).
//!
//! Architecture note: the hook process and the MCP / pipeline process that
//! records citations are **different processes**. The bridge is this
//! rolling JSON sidecar on disk, keyed by PROJECT (not session) so both
//! sides can derive the same path from the repo root via
//! `kimetsu_core::paths::user_cache_dir_for`.
//!
//! Narrow scope:
//! - Capped at [`MAX_ENTRIES`] entries.
//! - Only entries within the last [`WINDOW_SECS`] are kept (pruned on write).
//! - All I/O is best-effort; callers swallow errors.
//!
//! The pure functions (`prune_window`, `match_and_remove`) accept `now_secs`
//! as a parameter so they are unit-testable without touching the clock.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Rolling window: entries older than 2 hours are pruned.
pub const WINDOW_SECS: u64 = 2 * 60 * 60;
/// Hard cap on the number of entries retained after pruning.
pub const MAX_ENTRIES: usize = 200;

/// A single dropped-capsule record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DroppedEntry {
    pub memory_id: String,
    pub dropped_at: u64,
}

/// The full sidecar structure persisted to disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DroppedCapsuleState {
    #[serde(default)]
    pub entries: Vec<DroppedEntry>,
}

/// Current unix timestamp in seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the path for the dropped-capsule sidecar given the project
/// user cache dir (`kimetsu_core::paths::user_cache_dir_for(&repo_root)`).
pub fn sidecar_path(cache_dir: &Path) -> std::path::PathBuf {
    cache_dir.join("dropped-recent.json")
}

// ── Pure functions (testable without fs/clock) ────────────────────────────────

/// Remove entries older than `window_secs` before `now_secs` and cap at
/// [`MAX_ENTRIES`]. Stable insertion order is preserved (oldest first).
pub fn prune_window(
    entries: Vec<DroppedEntry>,
    now_secs: u64,
    window_secs: u64,
) -> Vec<DroppedEntry> {
    let cutoff = now_secs.saturating_sub(window_secs);
    let mut pruned: Vec<DroppedEntry> = entries
        .into_iter()
        .filter(|e| e.dropped_at >= cutoff)
        .collect();
    if pruned.len() > MAX_ENTRIES {
        let excess = pruned.len() - MAX_ENTRIES;
        pruned.drain(0..excess);
    }
    pruned
}

/// Check if `memory_id` appears in `entries`. If yes, remove it in-place
/// and return the matching entry. Returns `None` if not found.
pub fn match_and_remove(entries: &mut Vec<DroppedEntry>, memory_id: &str) -> Option<DroppedEntry> {
    entries
        .iter()
        .position(|e| e.memory_id == memory_id)
        .map(|pos| entries.remove(pos))
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

/// Best-effort load — any error (missing / corrupt) yields an empty state.
pub fn load(path: &Path) -> DroppedCapsuleState {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic write: serialise `state` to a sibling `.tmp` file, then rename
/// it over `path`.  Because rename is atomic on the same filesystem, the
/// reader always sees either the old file or the new one — never a torn
/// partial write.  Failures are swallowed (callers use best-effort I/O).
fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = fs::create_dir_all(parent);
    let Ok(text) = serde_json::to_string(value) else {
        return;
    };
    // Build a sibling temp path: <file>.tmp  (same dir → same filesystem).
    let tmp_path = path.with_extension("tmp");
    if fs::write(&tmp_path, &text).is_ok() {
        let _ = fs::rename(&tmp_path, path);
    }
}

/// Best-effort save — failures are swallowed.
pub fn save(path: &Path, state: &DroppedCapsuleState) {
    atomic_write_json(path, state);
}

/// Append new dropped memory ids to the sidecar (best-effort, write-back).
///
/// Loads the existing sidecar, appends each `memory_id` with `dropped_at`,
/// prunes the 2-hour window and the 200-entry cap, then writes back.
pub fn append_dropped(cache_dir: &Path, memory_ids: impl Iterator<Item = String>, dropped_at: u64) {
    let path = sidecar_path(cache_dir);
    let mut state = load(&path);
    for id in memory_ids {
        state.entries.push(DroppedEntry {
            memory_id: id,
            dropped_at,
        });
    }
    state.entries = prune_window(state.entries, dropped_at, WINDOW_SECS);
    save(&path, &state);
}

/// Best-effort regret check: look up `memory_id` in the sidecar,
/// prune stale entries, remove the matching entry (if found), write
/// back, and return the matching [`DroppedEntry`] so the caller can
/// emit a `retrieval.regret` event.
///
/// Returns `None` when the sidecar can't be read, the memory is not
/// present, or the entry is already outside the window.
pub fn take_if_dropped(cache_dir: &Path, memory_id: &str, now: u64) -> Option<DroppedEntry> {
    let path = sidecar_path(cache_dir);
    let mut state = load(&path);
    state.entries = prune_window(state.entries, now, WINDOW_SECS);
    let found = match_and_remove(&mut state.entries, memory_id);
    if found.is_some() {
        save(&path, &state);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, dropped_at: u64) -> DroppedEntry {
        DroppedEntry {
            memory_id: id.to_string(),
            dropped_at,
        }
    }

    #[test]
    fn prune_window_removes_old_entries() {
        let now = 1_000_000u64;
        let entries = vec![
            entry("old", now - WINDOW_SECS - 1), // outside window
            entry("fresh", now - 60),            // inside window
        ];
        let pruned = prune_window(entries, now, WINDOW_SECS);
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].memory_id, "fresh");
    }

    #[test]
    fn prune_window_caps_at_max_entries() {
        let now = 1_000_000u64;
        // One more than the cap, all fresh.
        let entries: Vec<DroppedEntry> = (0..=MAX_ENTRIES)
            .map(|i| entry(&format!("m{i}"), now - 10))
            .collect();
        let pruned = prune_window(entries, now, WINDOW_SECS);
        assert_eq!(pruned.len(), MAX_ENTRIES);
        // Oldest entry (m0) should have been evicted.
        assert!(!pruned.iter().any(|e| e.memory_id == "m0"));
    }

    #[test]
    fn prune_window_keeps_boundary_entry() {
        let now = 1_000_000u64;
        let entries = vec![
            entry("boundary", now - WINDOW_SECS), // exactly at cutoff — kept
            entry("outside", now - WINDOW_SECS - 1), // one second older — pruned
        ];
        let pruned = prune_window(entries, now, WINDOW_SECS);
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].memory_id, "boundary");
    }

    #[test]
    fn match_and_remove_finds_and_removes() {
        let mut entries = vec![entry("a", 100), entry("b", 200), entry("c", 300)];
        let found = match_and_remove(&mut entries, "b");
        assert!(found.is_some());
        assert_eq!(found.unwrap().memory_id, "b");
        assert_eq!(entries.len(), 2);
        assert!(!entries.iter().any(|e| e.memory_id == "b"));
    }

    #[test]
    fn match_and_remove_returns_none_when_absent() {
        let mut entries = vec![entry("a", 100)];
        assert!(match_and_remove(&mut entries, "missing").is_none());
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn match_and_remove_on_empty_is_safe() {
        let mut entries: Vec<DroppedEntry> = vec![];
        assert!(match_and_remove(&mut entries, "x").is_none());
    }

    #[test]
    fn prune_window_empty_input_is_safe() {
        let pruned = prune_window(vec![], 1_000_000, WINDOW_SECS);
        assert!(pruned.is_empty());
    }

    /// S4.3: `save` must write atomically (temp-then-rename) so the reader
    /// never sees a partially-written file.  After save the target must be
    /// readable AND the sibling `.tmp` must NOT remain on disk.
    #[test]
    fn save_is_atomic_no_tmp_leftover() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let cache_dir = std::env::temp_dir().join(format!("kimetsu-atomic-dc-test-{nanos}"));
        std::fs::create_dir_all(&cache_dir).expect("mkdir");
        let path = sidecar_path(&cache_dir);

        let state = DroppedCapsuleState {
            entries: vec![entry("mem-atomic", 999_999)],
        };
        save(&path, &state);

        // The target file must exist and be readable.
        let loaded = load(&path);
        assert_eq!(
            loaded.entries.len(),
            1,
            "saved state must be loadable after atomic write"
        );
        assert_eq!(loaded.entries[0].memory_id, "mem-atomic");

        // The sibling .tmp must have been consumed by the rename.
        let tmp_path = path.with_extension("tmp");
        assert!(
            !tmp_path.exists(),
            ".tmp sibling must not remain after atomic save"
        );

        let _ = std::fs::remove_dir_all(&cache_dir);
    }
}
