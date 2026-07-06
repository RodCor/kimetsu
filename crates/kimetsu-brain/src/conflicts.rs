//! Conflict listing + resolution across project and user brains.
//! Split out of `project.rs` (v2.5.1); re-exported by [`crate::project`].

use std::path::Path;

use kimetsu_core::KimetsuResult;

use crate::conflict;
use crate::lock::ProjectLock;
use crate::project::*;
use crate::user_brain;

/// v0.5.2: list open conflict-detection hits across the project brain
/// and (when enabled) the user brain. Each `ConflictReport` carries a
/// `source` label so the CLI can render which brain originated it —
/// resolve takes a separate code path per brain since the row only
/// lives in one DB.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScopedConflict {
    /// Either "project" or "user". Determines which DB `resolve_conflict`
    /// must target when the operator chooses to apply a resolution.
    pub source: String,
    #[serde(flatten)]
    pub report: conflict::ConflictReport,
}

/// Merge open conflicts from project + user brains. `limit` is applied
/// per-brain, so the worst case is `limit * 2` rows returned — the CLI
/// can re-truncate on display if needed.
pub fn list_conflicts(start: &Path, limit: u32) -> KimetsuResult<Vec<ScopedConflict>> {
    let mut out = Vec::new();
    let (_paths, config, project_conn) = load_project_readonly(start)?;
    for report in conflict::list_unresolved_conflicts(&project_conn, limit)? {
        out.push(ScopedConflict {
            source: "project".to_string(),
            report,
        });
    }
    // W3.3: honor config.kimetsu.use_user_brain with env override.
    if let Some(user_conn) =
        user_brain::open_user_brain_readonly_for_config(config.kimetsu.use_user_brain)?
    {
        for report in conflict::list_unresolved_conflicts(&user_conn, limit)? {
            out.push(ScopedConflict {
                source: "user".to_string(),
                report,
            });
        }
    }
    out.sort_by(|a, b| b.report.detected_at.cmp(&a.report.detected_at));
    Ok(out)
}

/// Resolve a single open conflict by id with one of `kept_new`,
/// `kept_existing`, or `kept_both`. The conflict can live in either
/// the project brain or the user brain — we try project first, and on
/// "not found" fall through to user. Returns Ok(true) if a row was
/// updated.
///
/// We deliberately don't emit a `memory.invalidated` trace event here
/// even though `kept_new` / `kept_existing` invalidates one side. The
/// `memory_conflicts` row IS the audit trail; double-recording would
/// duplicate state across two systems. Operators who want the trace-
/// event-style record can use `kimetsu brain memory invalidate` instead.
pub fn resolve_conflict(start: &Path, conflict_id: &str, resolution: &str) -> KimetsuResult<bool> {
    let (paths, config, project_conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "brain memory conflict resolve", None)?;
    if conflict::resolve_conflict(&project_conn, conflict_id, resolution)? {
        return Ok(true);
    }
    drop(project_conn); // release before opening user brain (avoid pseudo-conflict on flock semantics)
    // W3.3: honor config.kimetsu.use_user_brain with env override.
    if let Some(user_conn) = user_brain::open_user_brain_for_config(config.kimetsu.use_user_brain)?
    {
        return conflict::resolve_conflict(&user_conn, conflict_id, resolution);
    }
    Ok(false)
}
