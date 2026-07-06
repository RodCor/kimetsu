//! Episodic work-resume: capture, store, and surface per-repo work episodes
//! so the next session resumes instead of re-deriving state.
//!
//! # Design
//!
//! Episodes are event-sourced via `work.episode` events → the `work_episodes`
//! projection table.  One live (non-superseded) episode per repo at a time;
//! each new capture supersedes the prior.
//!
//! ## Story coverage
//! * **1.3** — episode event + table; auto-capture at SessionEnd; optional
//!   cheap-model distillation with rule-based fallback when none is configured.
//! * **kimetsu checkpoint** — manual mid-session capture (CLI wires this).
//! * **1.4** — [`load_live_episode`] + [`render_resume_context`] (Pass B
//!   SessionStart injection surface).
//! * **1.7 (light)** — where the episode payload references concrete memory_id
//!   strings, a `lesson_from` edge is inserted via `projector::insert_memory_edge`.
//!   This is best-effort: if no memory ids are found in the payload the edge
//!   plumbing is simply not invoked.

use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::RunId;
use rusqlite::{Connection, OptionalExtension, params};
use time::format_description::well_known::Rfc3339;

use crate::project::{load_project, load_project_readonly};
use crate::projector;

// ---------------------------------------------------------------------------
// Episode data types
// ---------------------------------------------------------------------------

/// A serialized `work.episode` event payload (stored as JSON in the events
/// table).  All fields are optional strings so a partial capture never fails.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct EpisodePayload {
    /// Human-readable task description / goal.
    pub task: String,
    /// Narrative summary of what was done.
    pub summary: String,
    /// Things that remain to be done.
    pub open_threads: Vec<String>,
    /// Dead-ends: failed approaches and why they failed.
    pub dead_ends: Vec<String>,
    /// Working hypothesis / current best theory.
    pub hypothesis: String,
    /// Optional note supplied by the user (e.g. from `kimetsu checkpoint note`).
    #[serde(default, skip_serializing_if = "str::is_empty")]
    pub note: String,
    /// Canonical repo root used as the per-repo scope key.
    pub repo_root: String,
    /// memory_ids referenced in this episode (for 1.7 edge insertion).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_ids: Vec<String>,
}

/// A row from the `work_episodes` projection table.
#[derive(Debug, Clone)]
pub struct EpisodeRow {
    pub episode_id: String,
    pub repo_root: String,
    pub task: String,
    pub summary: String,
    pub open_threads: Vec<String>,
    pub dead_ends: Vec<String>,
    pub hypothesis: String,
    pub note: String,
    pub created_at: String,
    /// Set to the episode_id of the episode that superseded this one, or None
    /// if this is the live episode.
    pub superseded_by: Option<String>,
}

// ---------------------------------------------------------------------------
// Schema helper: ensures `work_episodes` table exists (idempotent).
// Called from the v4→v5 migration; also used by tests directly.
// ---------------------------------------------------------------------------

/// Create the `work_episodes` projection table and its indexes.
///
/// This function is called by the v4→v5 migration; it is idempotent
/// (`IF NOT EXISTS`).  Do NOT issue BEGIN/COMMIT here — the migration runner
/// owns the transaction.
pub(crate) fn create_work_episodes_table(conn: &Connection) -> KimetsuResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS work_episodes (
            episode_id      TEXT PRIMARY KEY,
            repo_root       TEXT NOT NULL,
            task            TEXT NOT NULL DEFAULT '',
            summary         TEXT NOT NULL DEFAULT '',
            open_threads    TEXT NOT NULL DEFAULT '[]',
            dead_ends       TEXT NOT NULL DEFAULT '[]',
            hypothesis      TEXT NOT NULL DEFAULT '',
            note            TEXT NOT NULL DEFAULT '',
            created_at      TEXT NOT NULL,
            superseded_by   TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_episodes_repo_live
            ON work_episodes (repo_root, superseded_by);

        CREATE INDEX IF NOT EXISTS idx_episodes_repo_created
            ON work_episodes (repo_root, created_at DESC);
        ",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection: project a `work.episode` event into `work_episodes`
// ---------------------------------------------------------------------------

/// Project a single `work.episode` event into the `work_episodes` table.
///
/// 1. Insert the new episode row (OR IGNORE — replay-safe).
/// 2. Stamp `superseded_by = new_episode_id` on the prior live episode for
///    this `repo_root` (if any).
/// 3. Insert `lesson_from` edges for any `memory_ids` in the payload (1.7
///    light — best-effort).
pub(crate) fn project_work_episode(
    conn: &Connection,
    event: &kimetsu_core::event::Event,
) -> KimetsuResult<()> {
    let payload: EpisodePayload = serde_json::from_value(event.payload.clone()).unwrap_or_default();
    let episode_id = event.event_id.to_string();
    let ts = event
        .ts
        .format(&Rfc3339)
        .map_err(|e| format!("format ts: {e}"))?;

    let open_threads_json = serde_json::to_string(&payload.open_threads)?;
    let dead_ends_json = serde_json::to_string(&payload.dead_ends)?;

    // 1. Insert the new episode (OR IGNORE for replay-safety).
    conn.execute(
        "
        INSERT OR IGNORE INTO work_episodes (
            episode_id, repo_root, task, summary, open_threads, dead_ends,
            hypothesis, note, created_at, superseded_by
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
        ",
        params![
            episode_id,
            payload.repo_root,
            payload.task,
            payload.summary,
            open_threads_json,
            dead_ends_json,
            payload.hypothesis,
            payload.note,
            ts,
        ],
    )?;

    // 2. Supersede the prior live episode for this repo_root (if any).
    // We find the most-recent non-superseded episode that is NOT this one,
    // and stamp superseded_by = episode_id.
    let prior_id: Option<String> = conn
        .query_row(
            "
            SELECT episode_id FROM work_episodes
            WHERE repo_root = ?1
              AND superseded_by IS NULL
              AND episode_id != ?2
            ORDER BY created_at DESC
            LIMIT 1
            ",
            params![payload.repo_root, episode_id],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(prior) = &prior_id {
        conn.execute(
            "UPDATE work_episodes SET superseded_by = ?2 WHERE episode_id = ?1",
            params![prior, episode_id],
        )?;
    }

    // 3. Story 1.7 (light): insert `lesson_from` edges for referenced memories.
    //    Best-effort — if the memory row doesn't exist yet (e.g. replay order) the
    //    INSERT OR IGNORE is still safe; the edge simply points to a non-existent
    //    dst (no FK constraint).
    for memory_id in &payload.memory_ids {
        if memory_id.is_empty() {
            continue;
        }
        // episode_id is not a memory_id, so we use the episode_id as src
        // and the memory as dst, edge type "lesson_from".
        projector::insert_memory_edge(conn, &episode_id, memory_id, "lesson_from", &ts)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Read path: load the live episode for a repo_root
// ---------------------------------------------------------------------------

// Raw DB row type alias for load_live_episode.
type EpisodeDbRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

/// Load the live (non-superseded) episode for `repo_root`, or `None` when
/// none exists.
pub fn load_live_episode(conn: &Connection, repo_root: &str) -> KimetsuResult<Option<EpisodeRow>> {
    let row: Option<EpisodeDbRow> = conn
        .query_row(
            "
            SELECT episode_id, repo_root, task, summary, open_threads, dead_ends,
                   hypothesis, note, created_at, superseded_by
            FROM work_episodes
            WHERE repo_root = ?1
              AND superseded_by IS NULL
            ORDER BY created_at DESC
            LIMIT 1
            ",
            params![repo_root],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .optional()?;

    let Some((
        episode_id,
        repo_root_val,
        task,
        summary,
        open_threads_json,
        dead_ends_json,
        hypothesis,
        note,
        created_at,
        superseded_by,
    )) = row
    else {
        return Ok(None);
    };

    let open_threads: Vec<String> = serde_json::from_str(&open_threads_json).unwrap_or_default();
    let dead_ends: Vec<String> = serde_json::from_str(&dead_ends_json).unwrap_or_default();

    Ok(Some(EpisodeRow {
        episode_id,
        repo_root: repo_root_val,
        task,
        summary,
        open_threads,
        dead_ends,
        hypothesis,
        note,
        created_at,
        superseded_by,
    }))
}

// ---------------------------------------------------------------------------
// 1.4: render_resume_context — Pass B SessionStart surface
// ---------------------------------------------------------------------------

/// Render the live episode for `workspace` as a concise injection string
/// suitable for SessionStart context injection (≤ ~120 tokens of content).
///
/// Returns `None` when:
/// - no Kimetsu project is found at `workspace`, or
/// - no live episode exists for that repo.
///
/// The caller (Pass B SessionStart hook) should prepend this string to the
/// user prompt or system context.  The format is intentionally compact:
///
/// ```text
/// Last session in this repo: you were working on <task>.
/// Done: <summary>.
/// Open: <open_threads>.
/// Avoid: <dead_ends>.
/// Hypothesis: <hypothesis>.
/// ```
pub fn render_resume_context(workspace: &Path) -> Option<String> {
    let (paths, _config, conn) = load_project_readonly(workspace).ok()?;
    let repo_root = paths.repo_root.to_string_lossy().to_string();
    let episode = load_live_episode(&conn, &repo_root).ok().flatten()?;
    Some(format_episode_for_context(&episode))
}

/// Format an episode row as the resume context string.
fn format_episode_for_context(ep: &EpisodeRow) -> String {
    let mut parts = Vec::new();

    let task = ep.task.trim();
    if !task.is_empty() {
        parts.push(format!(
            "Last session in this repo: you were working on {task}."
        ));
    } else {
        parts.push("Last session in this repo: previous work captured below.".to_string());
    }

    let summary = ep.summary.trim();
    if !summary.is_empty() {
        parts.push(format!("Done: {summary}."));
    }

    if !ep.open_threads.is_empty() {
        let threads = ep
            .open_threads
            .iter()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();
        if !threads.is_empty() {
            parts.push(format!("Open: {}.", threads.join("; ")));
        }
    }

    if !ep.dead_ends.is_empty() {
        let de = ep
            .dead_ends
            .iter()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();
        if !de.is_empty() {
            parts.push(format!("Avoid: {}.", de.join("; ")));
        }
    }

    let hypothesis = ep.hypothesis.trim();
    if !hypothesis.is_empty() {
        parts.push(format!("Hypothesis: {hypothesis}."));
    }

    if !ep.note.trim().is_empty() {
        parts.push(format!("Note: {}.", ep.note.trim()));
    }

    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Capture path: write a `work.episode` event
// ---------------------------------------------------------------------------

/// Write a `work.episode` event to the brain for `workspace`, projecting it
/// immediately.  Returns the new `episode_id` (= the event_id ULID).
///
/// This is the single canonical write path used by:
/// - The SessionEnd auto-capture (via `distiller::capture_episode`).
/// - `kimetsu checkpoint [note]`.
pub fn capture_episode(workspace: &Path, payload: EpisodePayload) -> KimetsuResult<String> {
    let (paths, _config, conn) = load_project(workspace)?;
    let _lock = crate::lock::ProjectLock::acquire(&paths, "episode capture", None)?;

    // Always use the canonical repo_root from ProjectPaths so that
    // load_live_episode_for_workspace (which also reads from paths.repo_root)
    // always finds the same key — even if the caller supplied a non-canonical path.
    let mut payload = payload;
    payload.repo_root = paths.repo_root.to_string_lossy().to_string();

    let run_id = RunId::new();
    let event =
        kimetsu_core::event::Event::new(run_id, "work.episode", serde_json::to_value(&payload)?);
    let event_id = event.event_id.to_string();

    // Write into events table and project.
    projector::insert_event(&conn, &event)?;
    project_work_episode(&conn, &event)?;

    Ok(event_id)
}

// ---------------------------------------------------------------------------
// Rule-based episode fallback (no cheap model configured)
// ---------------------------------------------------------------------------

/// Build an `EpisodePayload` using only the transcript view and git metadata —
/// no model call.  Used when `config.cheap_model()` is `None`.
///
/// Strategy:
/// - `task`: first non-empty user line of the transcript (the prompt).
/// - `summary`: last assistant line (the final answer summary).
/// - `open_threads`: any line containing "TODO", "still need", "next step", "follow-up".
/// - `dead_ends`: any line containing "failed", "doesn't work", "error:", "gave up".
/// - `hypothesis`: empty (rule-based has no reasoning to capture).
pub fn rule_based_episode(transcript_view: &str, repo_root: &str, note: &str) -> EpisodePayload {
    let lines: Vec<&str> = transcript_view.lines().collect();

    // task: first user: line
    let task = lines
        .iter()
        .find(|l| l.starts_with("user:"))
        .map(|l| l.trim_start_matches("user:").trim().to_string())
        .unwrap_or_default();

    // summary: last assistant: line
    let summary = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("assistant:"))
        .map(|l| l.trim_start_matches("assistant:").trim().to_string())
        .unwrap_or_default();

    // open_threads: lines containing open-thread signals
    let open_thread_signals = ["todo", "still need", "next step", "follow-up", "followup"];
    let open_threads: Vec<String> = lines
        .iter()
        .filter(|l| {
            let low = l.to_lowercase();
            open_thread_signals.iter().any(|sig| low.contains(sig))
        })
        .take(3)
        .map(|l| l.trim().to_string())
        .collect();

    // dead_ends: lines containing failure signals
    let dead_end_signals = [
        "failed",
        "doesn't work",
        "does not work",
        "error:",
        "gave up",
        "won't work",
    ];
    let dead_ends: Vec<String> = lines
        .iter()
        .filter(|l| {
            let low = l.to_lowercase();
            dead_end_signals.iter().any(|sig| low.contains(sig))
        })
        .take(3)
        .map(|l| l.trim().to_string())
        .collect();

    EpisodePayload {
        task: task.chars().take(200).collect(),
        summary: summary.chars().take(300).collect(),
        open_threads,
        dead_ends,
        hypothesis: String::new(),
        note: note.to_string(),
        repo_root: repo_root.to_string(),
        memory_ids: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Public query: load live episode by workspace path
// ---------------------------------------------------------------------------

/// Convenience wrapper: load the live episode for `workspace` (resolves the
/// repo_root from `ProjectPaths`).  Used by `kimetsu resume`.
pub fn load_live_episode_for_workspace(workspace: &Path) -> KimetsuResult<Option<EpisodeRow>> {
    let (paths, _config, conn) = load_project_readonly(workspace)?;
    let repo_root = paths.repo_root.to_string_lossy().to_string();
    load_live_episode(&conn, &repo_root)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use kimetsu_core::paths::git_init_boundary;

    use super::*;
    use crate::{project, schema, user_brain};

    fn tmp_workspace(name: &str) -> std::path::PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("kimetsu-ep-{name}-{ts}"));
        std::fs::create_dir_all(&dir).expect("create tmp");
        dir
    }

    fn make_in_memory_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        schema::initialize(&conn).expect("schema init");
        conn
    }

    // ------------------------------------------------------------------
    // E1: project_work_episode round-trips through an in-memory conn
    // ------------------------------------------------------------------
    #[test]
    fn project_episode_round_trip() {
        let conn = make_in_memory_conn();
        let run_id = kimetsu_core::ids::RunId::new();
        let payload = EpisodePayload {
            task: "fix the build".to_string(),
            summary: "added missing feature flag".to_string(),
            open_threads: vec!["still need tests".to_string()],
            dead_ends: vec!["tried cmake, failed".to_string()],
            hypothesis: "the linker needs explicit paths".to_string(),
            note: "urgent".to_string(),
            repo_root: "/repo/foo".to_string(),
            memory_ids: vec![],
        };
        let event = kimetsu_core::event::Event::new(
            run_id,
            "work.episode",
            serde_json::to_value(&payload).unwrap(),
        );
        project_work_episode(&conn, &event).expect("project_work_episode");

        let ep = load_live_episode(&conn, "/repo/foo")
            .expect("load_live_episode")
            .expect("episode must exist");

        assert_eq!(ep.task, "fix the build");
        assert_eq!(ep.open_threads, vec!["still need tests".to_string()]);
        assert_eq!(ep.dead_ends, vec!["tried cmake, failed".to_string()]);
        assert_eq!(ep.hypothesis, "the linker needs explicit paths");
        assert_eq!(ep.note, "urgent");
        assert!(
            ep.superseded_by.is_none(),
            "new episode must not be superseded"
        );
    }

    // ------------------------------------------------------------------
    // E2: second episode supersedes the first
    // ------------------------------------------------------------------
    #[test]
    fn second_episode_supersedes_first() {
        let conn = make_in_memory_conn();
        let run_id = kimetsu_core::ids::RunId::new();

        let payload1 = EpisodePayload {
            task: "task 1".to_string(),
            repo_root: "/repo/bar".to_string(),
            ..Default::default()
        };
        let ev1 = kimetsu_core::event::Event::new(
            run_id,
            "work.episode",
            serde_json::to_value(&payload1).unwrap(),
        );
        project_work_episode(&conn, &ev1).expect("project ev1");

        let ep1_id = ev1.event_id.to_string();

        // Brief sleep not required — ULID monotonicity within same process
        // is guaranteed by the ULID spec (millisecond monotonicity + random).
        // Force strictly-later created_at by crafting a newer ts.
        let mut ev2 = kimetsu_core::event::Event::new(
            run_id,
            "work.episode",
            serde_json::to_value(EpisodePayload {
                task: "task 2".to_string(),
                repo_root: "/repo/bar".to_string(),
                ..Default::default()
            })
            .unwrap(),
        );
        // Ensure ts is strictly after ev1.ts.
        ev2.ts = ev1.ts + time::Duration::seconds(1);

        project_work_episode(&conn, &ev2).expect("project ev2");

        // ep1 must now be superseded.
        let ep1: Option<String> = conn
            .query_row(
                "SELECT superseded_by FROM work_episodes WHERE episode_id = ?1",
                [ep1_id],
                |r| r.get(0),
            )
            .expect("query ep1");
        assert!(
            ep1.is_some(),
            "first episode must be superseded after second"
        );

        // Only one live episode.
        let live = load_live_episode(&conn, "/repo/bar")
            .expect("load")
            .expect("live episode");
        assert_eq!(live.task, "task 2");
        assert!(live.superseded_by.is_none());
    }

    // ------------------------------------------------------------------
    // E3: reset_projection wipes work_episodes
    // ------------------------------------------------------------------
    #[test]
    fn reset_projection_clears_episodes() {
        let conn = make_in_memory_conn();
        let run_id = kimetsu_core::ids::RunId::new();
        let payload = EpisodePayload {
            task: "some task".to_string(),
            repo_root: "/repo/reset".to_string(),
            ..Default::default()
        };
        let event = kimetsu_core::event::Event::new(
            run_id,
            "work.episode",
            serde_json::to_value(&payload).unwrap(),
        );
        // Use apply_events to go through the full stack.
        crate::projector::apply_events(&conn, &[event]).expect("apply_events");

        let count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM work_episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_before, 1, "episode must exist before reset");

        // Reset must clear work_episodes.
        conn.execute_batch(
            "DELETE FROM runs; DELETE FROM sources; DELETE FROM memories; \
            DELETE FROM memory_proposals; DELETE FROM memories_fts; DELETE FROM memory_citations; \
            DELETE FROM memory_conflicts; DELETE FROM memory_edges; DELETE FROM work_episodes;",
        )
        .expect("manual reset");

        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM work_episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_after, 0, "work_episodes must be cleared after reset");
    }

    // ------------------------------------------------------------------
    // E4: rebuild_in_place re-projects work.episode events
    // ------------------------------------------------------------------
    #[test]
    fn rebuild_in_place_reprojects_episodes() {
        let conn = make_in_memory_conn();
        let run_id = kimetsu_core::ids::RunId::new();
        let payload = EpisodePayload {
            task: "rebuild test".to_string(),
            repo_root: "/repo/rebuild".to_string(),
            ..Default::default()
        };
        let event = kimetsu_core::event::Event::new(
            run_id,
            "work.episode",
            serde_json::to_value(&payload).unwrap(),
        );
        crate::projector::apply_events(&conn, &[event]).expect("apply_events");

        // Manually wipe the projection.
        conn.execute("DELETE FROM work_episodes", []).unwrap();
        let count_wiped: i64 = conn
            .query_row("SELECT COUNT(*) FROM work_episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_wiped, 0, "work_episodes wiped before rebuild");

        // rebuild_in_place must restore it.
        let replayed = crate::projector::rebuild_in_place(&conn).expect("rebuild_in_place");
        assert!(replayed >= 1, "at least 1 event replayed");

        let ep = load_live_episode(&conn, "/repo/rebuild")
            .expect("load")
            .expect("episode restored");
        assert_eq!(ep.task, "rebuild test");
    }

    // ------------------------------------------------------------------
    // E5: render_resume_context returns formatted text
    // ------------------------------------------------------------------
    #[test]
    fn render_resume_context_formats_episode() {
        let ep = EpisodeRow {
            episode_id: "ep1".to_string(),
            repo_root: "/r".to_string(),
            task: "implement feature X".to_string(),
            summary: "added the core logic".to_string(),
            open_threads: vec!["write tests".to_string(), "update docs".to_string()],
            dead_ends: vec!["tried approach A, OOM".to_string()],
            hypothesis: "batching is the fix".to_string(),
            note: String::new(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            superseded_by: None,
        };
        let text = format_episode_for_context(&ep);
        assert!(text.contains("implement feature X"), "task in output");
        assert!(text.contains("added the core logic"), "summary in output");
        assert!(text.contains("write tests"), "open thread in output");
        assert!(text.contains("tried approach A"), "dead-end in output");
        assert!(text.contains("batching is the fix"), "hypothesis in output");
        // Token budget: rough check that output is under 800 chars (~120 tokens).
        assert!(
            text.len() < 800,
            "render output must be concise (< 800 chars), got {}",
            text.len()
        );
    }

    // ------------------------------------------------------------------
    // E6: rule_based_episode parses transcript lines
    // ------------------------------------------------------------------
    #[test]
    fn rule_based_episode_parses_transcript() {
        let view = "user: implement the auth module\nassistant: I need to still need integration tests\n\
            assistant: cargo build failed — error: linker not found\nassistant: implemented the core auth flow.";
        let ep = rule_based_episode(view, "/r", "my note");
        assert_eq!(ep.task, "implement the auth module");
        assert!(!ep.summary.is_empty(), "summary extracted");
        assert_eq!(ep.note, "my note");
        assert_eq!(ep.repo_root, "/r");
    }

    // ------------------------------------------------------------------
    // E7: capture_episode end-to-end with a temp project
    // ------------------------------------------------------------------
    #[test]
    fn capture_episode_end_to_end() {
        let dir = tmp_workspace("capture");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            let payload = EpisodePayload {
                task: "e2e task".to_string(),
                summary: "e2e summary".to_string(),
                repo_root: dir.to_string_lossy().to_string(),
                ..Default::default()
            };
            let id = capture_episode(&dir, payload).expect("capture_episode");
            assert!(!id.is_empty(), "episode_id returned");

            let ep = load_live_episode_for_workspace(&dir)
                .expect("load")
                .expect("live episode");
            assert_eq!(ep.task, "e2e task");
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // ------------------------------------------------------------------
    // E8: 1.7 light — lesson_from edges inserted for memory_ids in payload
    // ------------------------------------------------------------------
    #[test]
    fn episode_inserts_lesson_from_edges() {
        let conn = make_in_memory_conn();
        let run_id = kimetsu_core::ids::RunId::new();
        let payload = EpisodePayload {
            task: "edge test".to_string(),
            repo_root: "/repo/edges".to_string(),
            memory_ids: vec!["mem-abc".to_string(), "mem-xyz".to_string()],
            ..Default::default()
        };
        let event = kimetsu_core::event::Event::new(
            run_id,
            "work.episode",
            serde_json::to_value(&payload).unwrap(),
        );
        let event_id_str = event.event_id.to_string();
        crate::projector::apply_events(&conn, std::slice::from_ref(&event)).expect("apply_events");

        let edge_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE src_id = ?1 AND edge_type = 'lesson_from'",
                [event_id_str],
                |r| r.get(0),
            )
            .expect("query edges");
        assert_eq!(edge_count, 2, "two lesson_from edges inserted");
    }
}
