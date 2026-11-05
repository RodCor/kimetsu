//! F0 mount grants and the seven local store operations.
//!
//! Session rows live in the project SQLite file, keyed by level and
//! `session_id`. Cite outcome is a field on the existing citation event:
//! omitted means `noop`. No signatures and no sockets.

use std::collections::HashSet;
use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope, MountLevel, mount_of_stored};

use crate::context::{ContextBundle, ContextRequest};
use crate::packs::{self, ImportSummary, MemoryExport, PackRef, ScrubReport};
use crate::project::{self, BrainSession, ProposeResult};

/// Read and write grants for one handle. Write includes read.
#[derive(Debug, Clone, Default)]
pub struct MountGrants {
    read: HashSet<MountLevel>,
    write: HashSet<MountLevel>,
}

impl MountGrants {
    pub fn allow_read(&mut self, level: MountLevel) {
        self.read.insert(level);
    }

    pub fn allow_write(&mut self, level: MountLevel) {
        self.write.insert(level);
        self.read.insert(level);
    }

    pub fn can_read(&self, level: MountLevel) -> bool {
        self.read.contains(&level)
    }

    pub fn can_write(&self, level: MountLevel) -> bool {
        self.write.contains(&level)
    }

    pub fn repo_read_only() -> Self {
        let mut grants = Self::default();
        grants.allow_read(MountLevel::Repo);
        grants
    }

    /// Local stdio MCP: read repo, read user through the read-only opener,
    /// and write repo only while write tools are on.
    pub fn stdio_mcp(write_tools: bool) -> Self {
        let mut grants = Self::default();
        grants.allow_read(MountLevel::Repo);
        grants.allow_read(MountLevel::User);
        if write_tools {
            grants.allow_write(MountLevel::Repo);
        }
        grants
    }

    pub fn reads(&self) -> Vec<MountLevel> {
        let mut levels: Vec<_> = self.read.iter().copied().collect();
        levels.sort_by_key(|level| level.as_str());
        levels
    }
}

fn scope_for(level: MountLevel) -> MemoryScope {
    match level {
        MountLevel::Run => MemoryScope::Run,
        MountLevel::Session => MemoryScope::Session,
        MountLevel::Repo => MemoryScope::Repo,
        MountLevel::Org => MemoryScope::Org,
        MountLevel::User => MemoryScope::GlobalUser,
        MountLevel::Skill => MemoryScope::Skill,
    }
}

/// `context`: read only the mounts this handle opened.
pub fn context(
    start: &Path,
    grants: &MountGrants,
    query: &str,
    budget_tokens: u32,
) -> KimetsuResult<ContextBundle> {
    let session = BrainSession::open_readonly(start)?;
    let request = ContextRequest {
        stage: "localization".to_string(),
        query: query.to_string(),
        budget_tokens,
        min_score: 0.0,
        readable_mounts: Some(grants.reads()),
        ..Default::default()
    };
    session.retrieve_context_with_request(request)
}

/// `record`: write on the named mount. New repo events store `repo`.
pub fn record(
    start: &Path,
    grants: &MountGrants,
    level: MountLevel,
    kind: MemoryKind,
    text: &str,
    confidence: f32,
    session_id: Option<&str>,
) -> KimetsuResult<ProposeResult> {
    if !grants.can_write(level) {
        return Err(format!("mount {level} is not writable").into());
    }
    if level == MountLevel::Session && session_id.unwrap_or("").is_empty() {
        return Err("session record requires session_id".into());
    }
    let scope = scope_for(level);
    if session_id.is_some() {
        if confidence < 0.7 {
            let id = project::propose_memory(start, scope, kind, text, confidence, "mount record")?;
            return Ok(ProposeResult::Proposed(id));
        }
        let id = project::add_memory_for_lane(start, scope, kind, text, None, None, session_id)?;
        return Ok(ProposeResult::Added(id));
    }
    project::propose_or_merge_memory(start, scope, kind, text, confidence, "mount record")
}

/// `cite`: outcome `helped | noop | harmed`. Omitted is `noop`.
pub fn cite(
    start: &Path,
    grants: &MountGrants,
    exposure_id: &str,
    memory_id: &str,
    outcome: Option<&str>,
    note: Option<&str>,
) -> KimetsuResult<()> {
    let (_, _, conn) = project::load_project_readonly(start)?;
    let stored: String = conn
        .query_row(
            "SELECT scope FROM memories WHERE memory_id = ?1",
            [memory_id],
            |row| row.get(0),
        )
        .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
            "memory is not readable under this handle".into()
        })?;
    let level = mount_of_stored(&stored).ok_or("memory is not readable under this handle")?;
    if !grants.can_write(level) {
        return Err(format!("cite requires write on {level}").into());
    }
    drop(conn);
    project::record_exposure_citation(start, exposure_id, memory_id, note, outcome)
}

/// `search`: a level filter may only narrow the open read grants.
pub fn search(
    start: &Path,
    grants: &MountGrants,
    query: &str,
    level_filter: Option<MountLevel>,
) -> KimetsuResult<ContextBundle> {
    let mut levels = grants.reads();
    if let Some(level) = level_filter {
        if !grants.can_read(level) {
            return Err(format!("search filter {level} is not readable").into());
        }
        levels = vec![level];
    }
    let session = BrainSession::open_readonly(start)?;
    let request = ContextRequest {
        stage: "localization".to_string(),
        query: query.to_string(),
        budget_tokens: 4000,
        min_score: 0.0,
        readable_mounts: Some(levels),
        ..Default::default()
    };
    session.retrieve_context_with_request(request)
}

/// `supersede`: one predecessor, one successor, one mount, one durable link.
pub fn supersede(
    start: &Path,
    grants: &MountGrants,
    predecessor: &str,
    successor: &str,
) -> KimetsuResult<()> {
    let (_, _, conn) = project::load_project_readonly(start)?;
    let stored: String = conn
        .query_row(
            "SELECT scope FROM memories WHERE memory_id = ?1",
            [predecessor],
            |row| row.get(0),
        )
        .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
            "missing predecessor".into()
        })?;
    let level = mount_of_stored(&stored).ok_or("missing predecessor")?;
    if !grants.can_write(level) {
        return Err(format!("supersede requires write on {level}").into());
    }
    drop(conn);
    project::supersede_memory(start, predecessor, successor)
}

/// `export_pack`: v1 gzip-compatible rows. Run and session stay out.
pub fn export_pack(
    start: &Path,
    grants: &MountGrants,
    level: Option<MountLevel>,
) -> KimetsuResult<(Vec<MemoryExport>, ScrubReport)> {
    if let Some(level) = level {
        if !grants.can_read(level) {
            return Err(format!("export requires read on {level}").into());
        }
        if matches!(level, MountLevel::Run | MountLevel::Session) {
            return Err("export_pack refuses run and session mounts".into());
        }
    }
    let scope = level.map(scope_for);
    packs::export_memories(start, scope, None, true, false)
}

/// `import_pack`: v1, no signature. Replace retires only the named mount
/// (repo also retires alias-window `project` rows).
pub fn import_pack(
    start: &Path,
    grants: &MountGrants,
    entries: &[MemoryExport],
    destination: MountLevel,
    replace: bool,
    quarantine: bool,
) -> KimetsuResult<ImportSummary> {
    if !grants.can_write(destination) {
        return Err(format!("import requires write on {destination}").into());
    }
    packs::import_pack(
        start,
        entries,
        Some(scope_for(destination)),
        replace,
        Some(&PackRef {
            name: Some("f0".into()),
            version: Some("1".into()),
        }),
        quarantine,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consolidate::{ConsolidateRow, find_merge_clusters};
    use crate::digest::{self, build_or_load_digest};
    use crate::episode::render_resume_context_scoped;
    use crate::projector;
    use crate::sync::is_sync_allowed;
    use crate::user_brain::{self, test_env_lock};
    use kimetsu_core::paths::git_init_boundary;
    use std::path::PathBuf;

    fn temp_project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kimetsu-f0-{name}-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn operator_repo() -> MountGrants {
        let mut grants = MountGrants::default();
        grants.allow_write(MountLevel::Repo);
        grants.allow_write(MountLevel::Session);
        grants.allow_write(MountLevel::User);
        grants
    }

    fn row(scope: &str, text: &str) -> ConsolidateRow {
        ConsolidateRow {
            memory_id: format!("{scope}-{text}"),
            scope: scope.to_string(),
            kind: "fact".into(),
            text: text.to_string(),
            use_count: 1,
            usefulness_score: 0.5,
            last_useful_at: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            embedding: vec![1.0, 0.0, 0.0],
            model_id: "test".into(),
        }
    }

    #[test]
    fn merge_refuses_cross_mount_and_alias_collapse() {
        let repo = row("repo", "same claim");
        let user = row("user", "same claim");
        assert!(
            find_merge_clusters(&[repo.clone(), user], 0.92).is_empty(),
            "repo and user must both remain"
        );
        let project = row("project", "same claim");
        assert!(
            find_merge_clusters(&[repo, project], 0.92).is_empty(),
            "alias window must not collapse project into repo"
        );
    }

    #[test]
    fn replace_retires_repo_alias_and_leaves_user() {
        user_brain::with_user_brain_disabled(|| {
            let root = temp_project("replace");
            project::init_project(&root, false).unwrap();
            let repo = project::add_memory(&root, MemoryScope::Repo, MemoryKind::Fact, "repo fact")
                .unwrap();
            let project_row = project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "legacy project",
            )
            .unwrap();
            // Direct event keeps the stored string `project` (alias window).
            // add_memory(Project) now stores `repo`, so plant the legacy row.
            let legacy = "legacy-project-row";
            {
                let (_, _, conn) = project::load_project(&root).unwrap();
                projector::apply_events(
                    &conn,
                    &[kimetsu_core::event::Event::new(
                        kimetsu_core::ids::RunId::new(),
                        "memory.accepted",
                        serde_json::json!({
                            "memory_id": legacy,
                            "text": "stored project row",
                            "scope": "project",
                            "kind": "fact"
                        }),
                    )],
                )
                .unwrap();
                projector::apply_events(
                    &conn,
                    &[kimetsu_core::event::Event::new(
                        kimetsu_core::ids::RunId::new(),
                        "memory.accepted",
                        serde_json::json!({
                            "memory_id": "user-row",
                            "text": "USER_ROW_STAYS",
                            "scope": "global_user",
                            "kind": "fact"
                        }),
                    )],
                )
                .unwrap();
            }
            let entries = vec![MemoryExport {
                text: "incoming repo pack".into(),
                scope: "repo".into(),
                kind: "fact".into(),
                confidence: 0.9,
                created_at: None,
            }];
            import_pack(
                &root,
                &operator_repo(),
                &entries,
                MountLevel::Repo,
                true,
                false,
            )
            .unwrap();
            let (_, _, conn) = project::load_project_readonly(&root).unwrap();
            let active = |id: &str| -> bool {
                conn.query_row(
                    "SELECT invalidated_at IS NULL AND superseded_by IS NULL FROM memories WHERE memory_id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
            };
            assert!(!active(&repo), "repo row should be retired");
            assert!(
                !active(legacy),
                "alias-window project row should be retired"
            );
            assert!(active("user-row"), "user row must stay active");
            let _ = project_row;
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn replay_preserves_session_level() {
        user_brain::with_user_brain_disabled(|| {
            let root = temp_project("replay");
            project::init_project(&root, false).unwrap();
            let session = record(
                &root,
                &operator_repo(),
                MountLevel::Session,
                MemoryKind::Fact,
                "session task title",
                0.9,
                Some("task-a"),
            )
            .unwrap();
            let session_id = match session {
                ProposeResult::Added(id) | ProposeResult::Duplicate(id) => id,
                other => panic!("expected an accepted session row, got {other:?}"),
            };
            let run = project::add_memory(
                &root,
                MemoryScope::Run,
                MemoryKind::Fact,
                "run telemetry row",
            )
            .unwrap();
            {
                let (_, _, conn) = project::load_project(&root).unwrap();
                projector::rebuild_in_place(&conn).unwrap();
            }
            let (_, _, conn) = project::load_project_readonly(&root).unwrap();
            let scope_of = |id: &str| -> String {
                conn.query_row(
                    "SELECT scope FROM memories WHERE memory_id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap()
            };
            assert_eq!(scope_of(&session_id), "session");
            assert_ne!(scope_of(&run), "session");
            let level_on_event: String = conn
                .query_row(
                    "SELECT json_extract(payload_json, '$.scope') FROM events
                     WHERE kind='memory.accepted' AND json_extract(payload_json, '$.memory_id')=?1",
                    [&session_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(level_on_event, "session");
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn warm_start_keeps_session_off_the_repo_digest() {
        let root = temp_project("warm");
        git_init_boundary(&root);
        let user_dir = root.join("user-brain");
        std::fs::create_dir_all(&user_dir).unwrap();
        let _guard = test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        let prev_enabled = std::env::var("KIMETSU_USER_BRAIN").ok();
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", &user_dir);
            std::env::remove_var("KIMETSU_USER_BRAIN");
        }
        project::init_project(&root, false).unwrap();
        project::add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Fact,
            "REPO_FACT_ZEBRA port is 4001",
        )
        .unwrap();
        record(
            &root,
            &operator_repo(),
            MountLevel::Session,
            MemoryKind::Fact,
            "SESSION_TITLE_ORCHID",
            0.9,
            Some("lane-1"),
        )
        .unwrap();
        let user = user_brain::open_user_brain().unwrap().unwrap();
        user_brain::add_user_memory(&user, MemoryKind::Preference, "USER_PREF_QUOKKA", 1.0)
            .unwrap();
        let before: i64 = user
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        drop(user);
        let digest = build_or_load_digest(&root, true).unwrap_or_default();
        let resume = render_resume_context_scoped(&root, "lane-1").unwrap_or_default();
        let profile = digest::user_profile_block(&root).unwrap_or_default();
        let user = user_brain::open_user_brain().unwrap().unwrap();
        let after: i64 = user
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert!(digest.contains("REPO_FACT_ZEBRA"), "{digest}");
        assert!(!digest.contains("SESSION_TITLE_ORCHID"), "{digest}");
        assert!(!digest.contains("USER_PREF_QUOKKA"), "{digest}");
        assert!(resume.contains("SESSION_TITLE_ORCHID"), "{resume}");
        assert!(!resume.contains("REPO_FACT_ZEBRA"), "{resume}");
        assert!(profile.contains("USER_PREF_QUOKKA"), "{profile}");
        assert_eq!(before, after, "warm-start user opener must not write");
        drop(user);
        unsafe {
            match prev_dir {
                Some(value) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", value),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_enabled {
                Some(value) => std::env::set_var("KIMETSU_USER_BRAIN", value),
                None => std::env::remove_var("KIMETSU_USER_BRAIN"),
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cite_helped_raises_usefulness_without_changing_level() {
        user_brain::with_user_brain_disabled(|| {
            let root = temp_project("cite");
            project::init_project(&root, false).unwrap();
            let id = project::add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Fact,
                "cite target fact",
            )
            .unwrap();
            let (before, revision): (f64, String) = {
                let (_, _, conn) = project::load_project_readonly(&root).unwrap();
                let score = conn
                    .query_row(
                        "SELECT usefulness_score FROM memories WHERE memory_id=?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .unwrap();
                let revision = projector::claim_revision_at(&conn, &id, None).unwrap();
                (score, revision)
            };
            let exposure = kimetsu_core::event::Event::new(
                kimetsu_core::ids::RunId::new(),
                "context.injected",
                serde_json::json!({
                    "memory_ids": [id],
                    "memory_revisions": { id.clone(): revision.clone() }
                }),
            );
            project::record_context_exposure(&root, &exposure).unwrap();
            cite(
                &root,
                &operator_repo(),
                &exposure.event_id.to_string(),
                &id,
                None,
                None,
            )
            .unwrap();
            let (noop_score, noop_scope): (f64, String) = {
                let (_, _, conn) = project::load_project_readonly(&root).unwrap();
                conn.query_row(
                    "SELECT usefulness_score, scope FROM memories WHERE memory_id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
            };
            assert!(
                (noop_score - before).abs() < 1e-9,
                "omitted outcome is noop"
            );
            assert_eq!(noop_scope, "repo");
            let err = cite(
                &root,
                &operator_repo(),
                &exposure.event_id.to_string(),
                &id,
                Some("helped"),
                None,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("conflict"), "{err}");
            cite(
                &root,
                &operator_repo(),
                &exposure.event_id.to_string(),
                &id,
                Some("noop"),
                None,
            )
            .unwrap();
            let exposure_helped = kimetsu_core::event::Event::new(
                kimetsu_core::ids::RunId::new(),
                "context.injected",
                serde_json::json!({
                    "memory_ids": [id],
                    "memory_revisions": { id.clone(): revision.clone() }
                }),
            );
            project::record_context_exposure(&root, &exposure_helped).unwrap();
            cite(
                &root,
                &operator_repo(),
                &exposure_helped.event_id.to_string(),
                &id,
                Some("helped"),
                None,
            )
            .unwrap();
            let (score, scope): (f64, String) = {
                let (_, _, conn) = project::load_project_readonly(&root).unwrap();
                conn.query_row(
                    "SELECT usefulness_score, scope FROM memories WHERE memory_id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
            };
            assert!(
                score > before,
                "helped may raise usefulness: {before} -> {score}"
            );
            assert_eq!(scope, "repo");
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn edit_correction_survives_rebuild_and_keeps_level() {
        user_brain::with_user_brain_disabled(|| {
            let root = temp_project("edit");
            project::init_project(&root, false).unwrap();
            let id = project::add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Fact,
                "original claim text",
            )
            .unwrap();
            project::edit_memory(&root, &id, Some("corrected claim text"), None).unwrap();
            {
                let (_, _, conn) = project::load_project(&root).unwrap();
                projector::rebuild_in_place(&conn).unwrap();
            }
            let (_, _, conn) = project::load_project_readonly(&root).unwrap();
            let (text, scope): (String, String) = conn
                .query_row(
                    "SELECT text, scope FROM memories WHERE memory_id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(text, "corrected claim text");
            assert_eq!(scope, "repo");
            let corrections: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE kind='memory.corrected'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(corrections, 1);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn supersede_links_one_memory_and_refuses_cross_mount() {
        user_brain::with_user_brain_disabled(|| {
            let root = temp_project("supersede");
            project::init_project(&root, false).unwrap();
            let old = project::add_memory(&root, MemoryScope::Repo, MemoryKind::Fact, "old rule")
                .unwrap();
            let new = project::add_memory(&root, MemoryScope::Repo, MemoryKind::Fact, "new rule")
                .unwrap();
            {
                let (_, _, conn) = project::load_project(&root).unwrap();
                projector::apply_events(
                    &conn,
                    &[kimetsu_core::event::Event::new(
                        kimetsu_core::ids::RunId::new(),
                        "memory.accepted",
                        serde_json::json!({
                            "memory_id": "user-keep",
                            "text": "user rule",
                            "scope": "global_user",
                            "kind": "fact"
                        }),
                    )],
                )
                .unwrap();
            }
            supersede(&root, &operator_repo(), &old, &new).unwrap();
            supersede(&root, &operator_repo(), &old, &new).unwrap();
            let err = supersede(&root, &operator_repo(), &new, "user-keep")
                .unwrap_err()
                .to_string();
            assert!(err.contains("one mount"), "{err}");
            let (_, _, conn) = project::load_project_readonly(&root).unwrap();
            let successor: String = conn
                .query_row(
                    "SELECT superseded_by FROM memories WHERE memory_id=?1",
                    [&old],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(successor, new);
            let events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE kind='memory.superseded'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                events, 1,
                "repeating the pair must not chain a second successor"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn sync_still_refuses_run_and_telemetry() {
        assert!(is_sync_allowed("memory.accepted"));
        assert!(is_sync_allowed("memory.cited"));
        assert!(is_sync_allowed("memory.superseded"));
        assert!(!is_sync_allowed("run.started"));
        assert!(!is_sync_allowed("run.finished"));
        assert!(!is_sync_allowed("context.injected"));
        assert!(!is_sync_allowed("work.episode"));
        assert!(!is_sync_allowed("digest_served"));
    }

    #[test]
    fn new_project_input_stores_repo_and_dedups_against_legacy_project() {
        user_brain::with_user_brain_disabled(|| {
            let root = temp_project("alias");
            project::init_project(&root, false).unwrap();
            {
                let (_, _, conn) = project::load_project(&root).unwrap();
                projector::apply_events(
                    &conn,
                    &[kimetsu_core::event::Event::new(
                        kimetsu_core::ids::RunId::new(),
                        "memory.accepted",
                        serde_json::json!({
                            "memory_id": "legacy",
                            "text": "Same normalized claim",
                            "normalized_text": "same normalized claim",
                            "scope": "project",
                            "kind": "fact"
                        }),
                    )],
                )
                .unwrap();
            }
            let id = project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "Same normalized claim",
            )
            .unwrap();
            assert_eq!(id, "legacy");
            {
                let (_, _, conn) = project::load_project_readonly(&root).unwrap();
                let scope: String = conn
                    .query_row(
                        "SELECT scope FROM memories WHERE memory_id='legacy'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(scope, "project", "dedup must not rewrite the stored string");
            }
            let fresh = project::add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "A brand new repo fact",
            )
            .unwrap();
            let (_, _, conn) = project::load_project_readonly(&root).unwrap();
            let stored: String = conn
                .query_row(
                    "SELECT scope FROM memories WHERE memory_id=?1",
                    [&fresh],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stored, "repo");
            std::fs::remove_dir_all(&root).ok();
        });
    }
}
