use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use kimetsu_core::memory::{MemoryKind, MemoryScope, normalize_memory_text};
use kimetsu_core::paths::{ProjectPaths, default_project_id};
use kimetsu_core::{KIMETSU_SCHEMA_VERSION, KimetsuResult};
use rusqlite::{Connection, params};
use ulid::Ulid;

use crate::context::{self, ContextBundle, ContextRequest};
use crate::ingest::{self, RepoIngestSummary};
use crate::lock::ProjectLock;
use crate::projector;
use crate::schema;
use crate::trace::{self, TraceWriter};

#[derive(Debug, Clone)]
pub struct InitSummary {
    pub project_id: String,
    pub repo_root: PathBuf,
    pub kimetsu_dir: PathBuf,
    pub brain_db: PathBuf,
    pub model: String,
    pub api_key_env: String,
    pub api_key_present: bool,
    pub wrote_project_toml: bool,
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: String,
    pub task: String,
    pub started_at: String,
    pub terminal_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub confidence: f32,
    pub use_count: u32,
}

#[derive(Debug, Clone)]
pub struct ProposalRow {
    pub proposal_id: String,
    pub run_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub rationale: String,
    pub proposed_confidence: f32,
    pub status: String,
}

pub fn init_project(start: &Path, force: bool) -> KimetsuResult<InitSummary> {
    let paths = ProjectPaths::discover(start)?;
    fs::create_dir_all(&paths.runs_dir)?;

    let project_id = default_project_id(&paths.repo_root);
    let config = ProjectConfig::default_for_project(project_id);
    let wrote_project_toml = if force || !paths.project_toml.exists() {
        fs::write(&paths.project_toml, config.to_toml()?)?;
        true
    } else {
        false
    };

    let config = load_config(&paths)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let api_key_present = resolve_env_value(&paths.repo_root, &config.model.api_key_env).is_some();

    Ok(InitSummary {
        project_id: config.kimetsu.project_id,
        repo_root: paths.repo_root,
        kimetsu_dir: paths.kimetsu_dir,
        brain_db: paths.brain_db,
        model: format!("{}/{}", config.model.provider, config.model.model),
        api_key_env: config.model.api_key_env,
        api_key_present,
        wrote_project_toml,
    })
}

pub fn load_project(start: &Path) -> KimetsuResult<(ProjectPaths, ProjectConfig, Connection)> {
    let paths = ProjectPaths::discover(start)?;
    let config = load_config(&paths)?;
    if config.kimetsu.schema_version != KIMETSU_SCHEMA_VERSION {
        return Err(format!(
            "project.toml schema version {} does not match expected {}",
            config.kimetsu.schema_version, KIMETSU_SCHEMA_VERSION
        )
        .into());
    }

    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;
    Ok((paths, config, conn))
}

pub fn load_config(paths: &ProjectPaths) -> KimetsuResult<ProjectConfig> {
    let content = fs::read_to_string(&paths.project_toml).map_err(|err| {
        format!(
            "failed to read {}; run `kimetsu init` first: {err}",
            paths.project_toml.display()
        )
    })?;
    ProjectConfig::from_toml(&content)
}

pub fn config_text(start: &Path) -> KimetsuResult<String> {
    let paths = ProjectPaths::discover(start)?;
    Ok(fs::read_to_string(paths.project_toml)?)
}

pub fn list_runs(start: &Path) -> KimetsuResult<Vec<RunSummary>> {
    let (_paths, _config, conn) = load_project(start)?;
    let mut stmt = conn.prepare(
        "
        SELECT run_id, task, started_at, terminal_kind
        FROM runs
        ORDER BY started_at DESC
        LIMIT 100
        ",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(RunSummary {
            run_id: row.get(0)?,
            task: row.get(1)?,
            started_at: row.get(2)?,
            terminal_kind: row.get(3)?,
        })
    })?;

    let mut runs = Vec::new();
    for row in rows {
        runs.push(row?);
    }
    Ok(runs)
}

pub fn show_run(start: &Path, run_id: &str) -> KimetsuResult<Option<RunSummary>> {
    let (_paths, _config, conn) = load_project(start)?;
    let mut stmt = conn.prepare(
        "
        SELECT run_id, task, started_at, terminal_kind
        FROM runs
        WHERE run_id = ?1
        ",
    )?;

    let mut rows = stmt.query(params![run_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(RunSummary {
            run_id: row.get(0)?,
            task: row.get(1)?,
            started_at: row.get(2)?,
            terminal_kind: row.get(3)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn add_memory(
    start: &Path,
    scope: MemoryScope,
    kind: MemoryKind,
    text: &str,
) -> KimetsuResult<String> {
    let (paths, config, conn) = load_project(start)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory add", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;
    let memory_id = Ulid::new().to_string();
    let normalized = normalize_memory_text(text);

    let started = Event::new(
        run_id,
        "run.started",
        serde_json::json!({
            "mode": "admin",
            "task": "memory add",
            "project_id": config.kimetsu.project_id,
            "repo_root": paths.repo_root.to_string_lossy(),
            "model": null,
            "platform": std::env::consts::OS,
            "kimetsu_version": env!("CARGO_PKG_VERSION"),
            "config_hash": config_hash(&paths.project_toml)?,
        }),
    );
    writer.append(&started, true)?;

    let accepted = Event::new(
        run_id,
        "memory.accepted",
        serde_json::json!({
            "proposal_id": null,
            "memory_id": memory_id,
            "scope": scope.to_string(),
            "kind": kind.to_string(),
            "text": text,
            "normalized_text": normalized,
            "confidence": 1.0,
            "provenance_snapshot": {
                "source": "manual_cli",
                "run_id": run_id.to_string(),
                "text": text,
            }
        }),
    );
    writer.append(&accepted, true)?;

    let finished = Event::new(
        run_id,
        "run.finished",
        serde_json::json!({
            "status": "success",
            "final_report_path": null,
            "total_cost_usd": 0.0,
            "total_tool_calls": 0,
        }),
    );
    writer.append(&finished, true)?;

    projector::apply_events(&conn, &[started, accepted, finished])?;

    Ok(memory_id)
}

pub fn list_memories(start: &Path) -> KimetsuResult<Vec<MemoryRow>> {
    let (_paths, _config, conn) = load_project(start)?;
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, scope, kind, text, confidence, use_count
        FROM memories
        ORDER BY created_at DESC
        LIMIT 100
        ",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(MemoryRow {
            memory_id: row.get(0)?,
            scope: row.get(1)?,
            kind: row.get(2)?,
            text: row.get(3)?,
            confidence: row.get(4)?,
            use_count: row.get(5)?,
        })
    })?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }
    Ok(memories)
}

pub fn list_proposals(start: &Path) -> KimetsuResult<Vec<ProposalRow>> {
    let (_paths, _config, conn) = load_project(start)?;
    let mut stmt = conn.prepare(
        "
        SELECT proposal_id, run_id, scope, kind, text, rationale,
               proposed_confidence, status
        FROM memory_proposals
        ORDER BY rowid DESC
        LIMIT 100
        ",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ProposalRow {
            proposal_id: row.get(0)?,
            run_id: row.get(1)?,
            scope: row.get(2)?,
            kind: row.get(3)?,
            text: row.get(4)?,
            rationale: row.get(5)?,
            proposed_confidence: row.get(6)?,
            status: row.get(7)?,
        })
    })?;

    let mut proposals = Vec::new();
    for row in rows {
        proposals.push(row?);
    }
    Ok(proposals)
}

pub fn ingest_repo(start: &Path) -> KimetsuResult<RepoIngestSummary> {
    let (paths, config, conn) = load_project(start)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain ingest-repo", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;

    let started = admin_started_event(&paths, &config, run_id, "repo ingest")?;
    writer.append(&started, true)?;

    let summary = ingest::ingest_repo(&conn, &paths, &config)?;

    let ingested = Event::new(
        run_id,
        "repo.ingested",
        serde_json::json!({
            "repo_root": summary.repo_root.to_string_lossy(),
            "indexed_files": summary.indexed_files,
            "skipped_files": summary.skipped_files,
            "manifests": summary.manifests,
        }),
    );
    writer.append(&ingested, true)?;

    let finished = admin_finished_event(run_id);
    writer.append(&finished, true)?;
    projector::apply_events(&conn, &[started, ingested, finished])?;

    Ok(summary)
}

pub fn search_files(
    start: &Path,
    query: &str,
    limit: u32,
) -> KimetsuResult<Vec<context::ContextCapsule>> {
    let (paths, _config, conn) = load_project(start)?;
    let repo_root = paths
        .repo_root
        .canonicalize()?
        .to_string_lossy()
        .to_string();
    context::search_repo_files(&conn, &repo_root, query, limit)
}

pub fn retrieve_context(
    start: &Path,
    stage: &str,
    query: &str,
    budget_tokens: u32,
) -> KimetsuResult<ContextBundle> {
    let (paths, config, conn) = load_project(start)?;
    let repo_root = paths
        .repo_root
        .canonicalize()?
        .to_string_lossy()
        .to_string();
    context::retrieve_context(
        &conn,
        &repo_root,
        &config.broker.weights,
        ContextRequest {
            stage: stage.to_string(),
            query: query.to_string(),
            budget_tokens,
        },
    )
}

pub fn accept_proposal(start: &Path, proposal_id: &str) -> KimetsuResult<String> {
    let (paths, config, conn) = load_project(start)?;
    let proposal = load_pending_proposal(&conn, proposal_id)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory accept", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;
    let memory_id = Ulid::new().to_string();
    let normalized = normalize_memory_text(&proposal.text);

    let started = admin_started_event(&paths, &config, run_id, "memory accept")?;
    writer.append(&started, true)?;

    let accepted = Event::new(
        run_id,
        "memory.accepted",
        serde_json::json!({
            "proposal_id": proposal.proposal_id,
            "memory_id": memory_id,
            "scope": proposal.scope,
            "kind": proposal.kind,
            "text": proposal.text,
            "normalized_text": normalized,
            "confidence": proposal.proposed_confidence,
            "provenance_snapshot": {
                "source": "memory_proposal",
                "proposal_id": proposal.proposal_id,
                "source_run_id": proposal.run_id,
            }
        }),
    );
    writer.append(&accepted, true)?;

    let finished = admin_finished_event(run_id);
    writer.append(&finished, true)?;

    projector::apply_events(&conn, &[started, accepted.clone(), finished])?;
    conn.execute(
        "
        UPDATE memory_proposals
        SET status = 'accepted',
            decided_at = ?2,
            decided_by = 'cli'
        WHERE proposal_id = ?1
        ",
        params![
            proposal_id,
            accepted
                .ts
                .format(&time::format_description::well_known::Rfc3339)?
        ],
    )?;

    Ok(memory_id)
}

pub fn reject_proposal(start: &Path, proposal_id: &str) -> KimetsuResult<()> {
    let (paths, config, conn) = load_project(start)?;
    let _proposal = load_pending_proposal(&conn, proposal_id)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory reject", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;

    let started = admin_started_event(&paths, &config, run_id, "memory reject")?;
    writer.append(&started, true)?;

    let rejected = Event::new(
        run_id,
        "memory.rejected",
        serde_json::json!({
            "proposal_id": proposal_id,
            "reason": "rejected_by_cli",
        }),
    );
    writer.append(&rejected, true)?;

    let finished = admin_finished_event(run_id);
    writer.append(&finished, true)?;

    projector::apply_events(&conn, &[started, rejected, finished])?;
    Ok(())
}

pub fn rebuild_projection(start: &Path) -> KimetsuResult<usize> {
    let (paths, _config, conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "brain rebuild", None)?;
    let events = trace::read_all_traces(&paths)?;
    projector::rebuild(&conn, &events)?;
    Ok(events.len())
}

pub fn clear_lock(start: &Path) -> KimetsuResult<bool> {
    let paths = ProjectPaths::discover(start)?;
    crate::lock::clear_force(&paths)
}

fn load_pending_proposal(conn: &Connection, proposal_id: &str) -> KimetsuResult<ProposalRow> {
    let mut stmt = conn.prepare(
        "
        SELECT proposal_id, run_id, scope, kind, text, rationale,
               proposed_confidence, status
        FROM memory_proposals
        WHERE proposal_id = ?1
        ",
    )?;
    let mut rows = stmt.query(params![proposal_id])?;
    let Some(row) = rows.next()? else {
        return Err(format!("memory proposal not found: {proposal_id}").into());
    };

    let proposal = ProposalRow {
        proposal_id: row.get(0)?,
        run_id: row.get(1)?,
        scope: row.get(2)?,
        kind: row.get(3)?,
        text: row.get(4)?,
        rationale: row.get(5)?,
        proposed_confidence: row.get(6)?,
        status: row.get(7)?,
    };

    if proposal.status != "pending" {
        return Err(format!(
            "memory proposal {proposal_id} is {}, not pending",
            proposal.status
        )
        .into());
    }

    Ok(proposal)
}

fn admin_started_event(
    paths: &ProjectPaths,
    config: &ProjectConfig,
    run_id: RunId,
    task: &str,
) -> KimetsuResult<Event> {
    Ok(Event::new(
        run_id,
        "run.started",
        serde_json::json!({
            "mode": "admin",
            "task": task,
            "project_id": config.kimetsu.project_id,
            "repo_root": paths.repo_root.to_string_lossy(),
            "model": null,
            "platform": std::env::consts::OS,
            "kimetsu_version": env!("CARGO_PKG_VERSION"),
            "config_hash": config_hash(&paths.project_toml)?,
        }),
    ))
}

fn admin_finished_event(run_id: RunId) -> Event {
    Event::new(
        run_id,
        "run.finished",
        serde_json::json!({
            "status": "success",
            "final_report_path": null,
            "total_cost_usd": 0.0,
            "total_tool_calls": 0,
        }),
    )
}

fn config_hash(path: &Path) -> KimetsuResult<String> {
    let bytes = fs::read(path)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn memory_add_survives_projection_rebuild_from_trace() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");

        init_project(&root, false).expect("init project");
        let memory_id = add_memory(
            &root,
            MemoryScope::GlobalUser,
            MemoryKind::Preference,
            "User prefers Rust for core infrastructure.",
        )
        .expect("add memory");

        let memories = list_memories(&root).expect("list memories");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory_id, memory_id);

        let event_count = rebuild_projection(&root).expect("rebuild projection");
        assert_eq!(event_count, 3);

        let memories = list_memories(&root).expect("list rebuilt memories");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].memory_id, memory_id);
        assert_eq!(
            memories[0].text,
            "User prefers Rust for core infrastructure."
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    #[test]
    fn repo_ingest_indexes_searchable_files_and_context_capsules() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(root.join("src")).expect("create src");
        fs::create_dir_all(root.join("target")).expect("create target");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        fs::write(
            root.join("src").join("lib.rs"),
            "pub fn rebuild_projection_memory() -> &'static str { \"projection rebuild\" }\n",
        )
        .expect("write source");
        fs::write(
            root.join("target").join("generated.rs"),
            "projection rebuild",
        )
        .expect("write skipped");
        fs::write(root.join(".env"), "TOKEN=secret").expect("write secret");
        fs::write(root.join("blob.bin"), b"abc\0def").expect("write binary");

        init_project(&root, false).expect("init project");
        add_memory(
            &root,
            MemoryScope::GlobalUser,
            MemoryKind::Preference,
            "User prefers Rust for core infrastructure.",
        )
        .expect("add memory");

        let summary = ingest_repo(&root).expect("ingest repo");
        assert_eq!(summary.indexed_files, 2);
        assert_eq!(summary.manifests, 1);

        let matches = search_files(&root, "projection rebuild", 5).expect("search files");
        assert!(
            matches
                .iter()
                .any(|capsule| capsule.expansion_handle == "file:src/lib.rs"),
            "expected src/lib.rs in search results: {matches:?}"
        );
        assert!(
            matches
                .iter()
                .all(|capsule| !capsule.expansion_handle.contains("target/")),
            "target files must not be indexed: {matches:?}"
        );

        let context =
            retrieve_context(&root, "localization", "Rust infrastructure", 1200).expect("context");
        assert!(
            context
                .capsules
                .iter()
                .any(|capsule| capsule.expansion_handle.starts_with("memory:")),
            "expected memory capsule in context: {:?}",
            context.capsules
        );

        rebuild_projection(&root).expect("rebuild projection");
        let matches = search_files(&root, "projection rebuild", 5).expect("search after rebuild");
        assert!(
            matches
                .iter()
                .any(|capsule| capsule.expansion_handle == "file:src/lib.rs"),
            "repo index should survive event-only rebuild: {matches:?}"
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }
}
