use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use kimetsu_core::memory::{MemoryKind, MemoryScope, normalize_memory_text};
use kimetsu_core::paths::{ProjectPaths, default_project_id};
use kimetsu_core::{KIMETSU_SCHEMA_VERSION, KimetsuResult};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use ulid::Ulid;

use crate::benchmark;
use crate::context::{self, ContextBundle, ContextRequest};
use crate::ingest::{self, RepoIngestSummary};
use crate::lock::ProjectLock;
use crate::projector;
use crate::schema;
use crate::trace::{self, TraceWriter};
use crate::user_brain;

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
    /// MP-4a: running net outcome score. +1 for each run.finished that
    /// surfaced this memory; -1 for each run.failed (excluding Gate
    /// failures). Use_count tracks all updates, useful as a small-sample
    /// guard before letting the score bias retrieval.
    pub usefulness_score: f32,
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
    pub decided_reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProposalFilter {
    pub scope: Option<String>,
    pub kind: Option<String>,
    pub from_run: Option<String>,
    pub min_confidence: Option<f32>,
    pub status: Option<String>,
    pub limit: u32,
}

#[derive(Debug, Clone, Default)]
pub struct AcceptOverrides {
    pub scope: Option<String>,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct RecordedBenchmarkOutcome {
    pub memory_id: String,
    pub task_slug: Option<String>,
    pub kind: MemoryKind,
    pub text: String,
    pub proposal_id: Option<String>,
    pub proposal_text: Option<String>,
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

pub fn load_project_readonly(
    start: &Path,
) -> KimetsuResult<(ProjectPaths, ProjectConfig, Connection)> {
    let paths = ProjectPaths::discover(start)?;
    let config = load_config(&paths)?;
    if config.kimetsu.schema_version != KIMETSU_SCHEMA_VERSION {
        return Err(format!(
            "project.toml schema version {} does not match expected {}",
            config.kimetsu.schema_version, KIMETSU_SCHEMA_VERSION
        )
        .into());
    }

    let conn = Connection::open_with_flags(&paths.brain_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    schema::validate(&conn)?;
    Ok((paths, config, conn))
}

pub struct BrainSession {
    paths: ProjectPaths,
    config: ProjectConfig,
    conn: Connection,
    /// v0.4.1: user-scope brain at `~/.kimetsu/brain.db`. Opened
    /// lazily during session construction; `None` when the user
    /// brain is disabled (`KIMETSU_USER_BRAIN=0`), no home dir is
    /// resolvable, or — for the read-only constructor — the file
    /// hasn't been created yet. Retrieval merges memories from this
    /// connection alongside the project DB; repo files and manifests
    /// stay project-only.
    user_conn: Option<Connection>,
    repo_root: String,
}

impl BrainSession {
    pub fn open(start: &Path) -> KimetsuResult<Self> {
        let (paths, config, conn) = load_project(start)?;
        // Read/write user brain — created on demand so a v0.4 binary
        // running on a v0.3 home dir provisions the file the first
        // time the user actually writes a GlobalUser memory.
        let user_conn = user_brain::open_user_brain()?;
        Self::from_parts(paths, config, conn, user_conn)
    }

    pub fn open_readonly(start: &Path) -> KimetsuResult<Self> {
        let (paths, config, conn) = load_project_readonly(start)?;
        // Read-only path skips file creation — if the user brain
        // doesn't exist yet we just retrieve from the project DB
        // alone, no surprise file under $HOME.
        let user_conn = user_brain::open_user_brain_readonly()?;
        Self::from_parts(paths, config, conn, user_conn)
    }

    fn from_parts(
        paths: ProjectPaths,
        config: ProjectConfig,
        conn: Connection,
        user_conn: Option<Connection>,
    ) -> KimetsuResult<Self> {
        let repo_root = paths
            .repo_root
            .canonicalize()?
            .to_string_lossy()
            .to_string();
        Ok(Self {
            paths,
            config,
            conn,
            user_conn,
            repo_root,
        })
    }

    pub fn retrieve_context(
        &self,
        stage: &str,
        query: &str,
        budget_tokens: u32,
    ) -> KimetsuResult<ContextBundle> {
        // v0.4.1: merge user-brain memories into the candidate set
        // when the user DB is open. The multi-conn path normalizes
        // both candidate streams together so a user-brain capsule
        // and a project-brain capsule are comparable on the same
        // `raw_relevance` scale before scoring.
        let extras: Vec<&Connection> = self.user_conn.as_ref().into_iter().collect();
        context::retrieve_context_multi(
            &self.conn,
            &self.repo_root,
            &self.config.broker.weights,
            ContextRequest {
                stage: stage.to_string(),
                query: query.to_string(),
                budget_tokens,
            },
            &extras,
        )
    }

    pub fn repo_root(&self) -> &Path {
        &self.paths.repo_root
    }

    /// v0.4.1: expose the user-brain connection so callers (e.g.
    /// `kimetsu brain status`) can report counts/paths without
    /// re-opening the file. Returns None when the user brain is
    /// disabled or unresolvable.
    pub fn user_conn(&self) -> Option<&Connection> {
        self.user_conn.as_ref()
    }
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
    // v0.4.1: GlobalUser memories route to `~/.kimetsu/brain.db` when
    // the user brain is enabled. The user-brain write path is
    // intentionally simpler (no run rows, no trace events, no project
    // lock) because there's no project to attribute them to.
    //
    // If the user brain is disabled (KIMETSU_USER_BRAIN=0) OR
    // unreachable (no $HOME), fall through to the project DB so
    // backward compat is preserved — existing scripts that wrote
    // GlobalUser memories into the project keep working.
    if scope == MemoryScope::GlobalUser
        && let Some(user_conn) = user_brain::open_user_brain()?
    {
        return user_brain::add_user_memory(&user_conn, kind, text, 1.0);
    }
    let (paths, config, conn) = load_project(start)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory add", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;
    let memory_id = Ulid::new().to_string();
    let normalized = normalize_memory_text(text);

    // MP-17 #14: dedup. If an ACTIVE memory with the same scope + kind +
    // normalized text already exists, return its ID without writing a
    // duplicate. The scope/kind tuple keeps task-specific duplicates
    // separate from global ones; the normalized form makes minor
    // whitespace / punctuation differences collapse to the same row.
    let existing: Option<String> = conn
        .query_row(
            "
            SELECT memory_id FROM memories
            WHERE scope = ?1 AND kind = ?2 AND normalized_text = ?3
              AND invalidated_at IS NULL
            LIMIT 1
            ",
            rusqlite::params![scope.to_string(), kind.to_string(), normalized],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing_id) = existing {
        return Ok(existing_id);
    }

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
    let mut memories = list_memories_from_conn(&conn)?;
    // v0.4.1: merge user-brain rows so the user sees their portable
    // capsules alongside the per-repo ones. We read user brain via
    // the read-only path (no file-create side-effect on list).
    if let Some(user_conn) = user_brain::open_user_brain_readonly()? {
        memories.extend(user_brain::list_user_memories(&user_conn)?);
    }
    Ok(memories)
}

fn list_memories_from_conn(conn: &Connection) -> KimetsuResult<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
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
            usefulness_score: row.get::<_, f64>(6)? as f32,
        })
    })?;

    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }
    Ok(memories)
}

/// MP-6: ranked list of memories sorted by the same usefulness ratio the
/// broker uses for retrieval scoring (`usefulness_score / use_count`).
/// Filters out invalidated rows and any memory with `use_count < min_uses`
/// (the small-sample guard; default 3 matches the broker's
/// SMALL_SAMPLE_THRESHOLD). Optional scope filter narrows to a single
/// memory class. Lets the user see which memories are actually doing
/// work so they can prune the rest with `memory prune`.
#[derive(Debug, Clone, Default)]
pub struct TopOptions {
    pub scope: Option<String>,
    pub min_uses: u32,
    pub limit: u32,
}

pub fn list_memories_top(start: &Path, opts: TopOptions) -> KimetsuResult<Vec<MemoryRow>> {
    let (_paths, _config, conn) = load_project(start)?;
    let min_uses = opts.min_uses.max(1) as i64;
    let limit = if opts.limit == 0 { 20 } else { opts.limit } as i64;

    let (sql, scope_param): (&str, Option<String>) = if let Some(scope) = opts.scope.as_deref() {
        (
            "
            SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
            FROM memories
            WHERE invalidated_at IS NULL
              AND use_count >= ?1
              AND lower(scope) = lower(?2)
            ORDER BY (usefulness_score / CAST(use_count AS REAL)) DESC, use_count DESC
            LIMIT ?3
            ",
            Some(scope.to_string()),
        )
    } else {
        (
            "
            SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
            FROM memories
            WHERE invalidated_at IS NULL
              AND use_count >= ?1
            ORDER BY (usefulness_score / CAST(use_count AS REAL)) DESC, use_count DESC
            LIMIT ?2
            ",
            None,
        )
    };

    let mut stmt = conn.prepare(sql)?;
    let mut rows = if let Some(scope) = scope_param {
        stmt.query_map(params![min_uses, scope, limit], map_memory_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![min_uses, limit], map_memory_row)?
            .collect::<Result<Vec<_>, _>>()?
    };

    // SQLite's NaN-from-zero protection: a freshly-created memory with
    // use_count=0 would division-zero, but the WHERE clause guards
    // min_uses >= 1, so we never see a NaN here. Sort is a defensive
    // tie-breaker only.
    rows.sort_by(|a, b| {
        let ra = a.usefulness_score as f64 / a.use_count.max(1) as f64;
        let rb = b.usefulness_score as f64 / b.use_count.max(1) as f64;
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(rows)
}

fn map_memory_row(row: &rusqlite::Row) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        memory_id: row.get(0)?,
        scope: row.get(1)?,
        kind: row.get(2)?,
        text: row.get(3)?,
        confidence: row.get(4)?,
        use_count: row.get(5)?,
        usefulness_score: row.get::<_, f64>(6)? as f32,
    })
}

/// MP-6: bulk prune of memories whose outcome-attribution data says they
/// are net-negative. Selection rules:
///   use_count >= min_uses
///   usefulness_score / use_count <= max_ratio
///   invalidated_at IS NULL
///   scope filter optional
///
/// `apply = false` is the default at the CLI layer so the user sees
/// what would be touched before any writes. `apply = true` invalidates
/// each match via the existing `invalidate_memory` path so every
/// removal still emits a canonical `memory.invalidated` event.
#[derive(Debug, Clone)]
pub struct PruneOptions {
    pub scope: Option<String>,
    pub min_uses: u32,
    pub max_ratio: f32,
    pub apply: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            scope: None,
            min_uses: 3,
            max_ratio: -0.2,
            apply: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PruneCandidate {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub use_count: u32,
    pub usefulness_score: f32,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct PruneSummary {
    pub candidates: Vec<PruneCandidate>,
    pub invalidated: u32,
    pub failed: u32,
}

pub fn prune_low_usefulness(start: &Path, opts: PruneOptions) -> KimetsuResult<PruneSummary> {
    let min_uses = opts.min_uses.max(1) as i64;

    let candidates = {
        let (_paths, _config, conn) = load_project(start)?;
        let (sql, scope_param): (&str, Option<String>) = if let Some(scope) = opts.scope.as_deref()
        {
            (
                "
                SELECT memory_id, scope, kind, text, use_count, usefulness_score
                FROM memories
                WHERE invalidated_at IS NULL
                  AND use_count >= ?1
                  AND (usefulness_score / CAST(use_count AS REAL)) <= ?2
                  AND lower(scope) = lower(?3)
                ORDER BY (usefulness_score / CAST(use_count AS REAL)) ASC
                ",
                Some(scope.to_string()),
            )
        } else {
            (
                "
                SELECT memory_id, scope, kind, text, use_count, usefulness_score
                FROM memories
                WHERE invalidated_at IS NULL
                  AND use_count >= ?1
                  AND (usefulness_score / CAST(use_count AS REAL)) <= ?2
                ORDER BY (usefulness_score / CAST(use_count AS REAL)) ASC
                ",
                None,
            )
        };
        let mut stmt = conn.prepare(sql)?;
        let max_ratio = opts.max_ratio as f64;
        let mut found: Vec<PruneCandidate> = if let Some(scope) = scope_param {
            stmt.query_map(params![min_uses, max_ratio, scope], |row| {
                Ok(PruneCandidate {
                    memory_id: row.get(0)?,
                    scope: row.get(1)?,
                    kind: row.get(2)?,
                    text: row.get(3)?,
                    use_count: row.get(4)?,
                    usefulness_score: row.get::<_, f64>(5)? as f32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![min_uses, max_ratio], |row| {
                Ok(PruneCandidate {
                    memory_id: row.get(0)?,
                    scope: row.get(1)?,
                    kind: row.get(2)?,
                    text: row.get(3)?,
                    use_count: row.get(4)?,
                    usefulness_score: row.get::<_, f64>(5)? as f32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        // Stable tie-break: lowest ratio first, then highest use_count
        // first (penalize the long-running underperformers).
        found.sort_by(|a, b| {
            let ra = a.usefulness_score as f64 / a.use_count.max(1) as f64;
            let rb = b.usefulness_score as f64 / b.use_count.max(1) as f64;
            ra.partial_cmp(&rb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.use_count.cmp(&a.use_count))
        });
        found
    };

    let mut summary = PruneSummary {
        candidates: candidates.clone(),
        invalidated: 0,
        failed: 0,
    };
    if !opts.apply {
        return Ok(summary);
    }

    for candidate in &candidates {
        let ratio = candidate.usefulness_score / candidate.use_count.max(1) as f32;
        let reason = format!(
            "pruned_by_usefulness ratio={:+.2} use_count={}",
            ratio, candidate.use_count
        );
        match invalidate_memory(start, &candidate.memory_id, Some(&reason)) {
            Ok(()) => summary.invalidated += 1,
            Err(_) => summary.failed += 1,
        }
    }
    Ok(summary)
}

pub fn list_proposals(start: &Path, filter: ProposalFilter) -> KimetsuResult<Vec<ProposalRow>> {
    let (_paths, _config, conn) = load_project(start)?;
    let mut sql = String::from(
        "
        SELECT proposal_id, run_id, scope, kind, text, rationale,
               proposed_confidence, status, decided_reason
        FROM memory_proposals
        ",
    );
    let mut clauses = Vec::<String>::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(scope) = filter.scope.as_deref() {
        clauses.push("scope = ?".to_string());
        params.push(Box::new(scope.to_string()));
    }
    if let Some(kind) = filter.kind.as_deref() {
        clauses.push("kind = ?".to_string());
        params.push(Box::new(kind.to_string()));
    }
    if let Some(run_id) = filter.from_run.as_deref() {
        clauses.push("run_id = ?".to_string());
        params.push(Box::new(run_id.to_string()));
    }
    if let Some(min_conf) = filter.min_confidence {
        clauses.push("proposed_confidence >= ?".to_string());
        params.push(Box::new(min_conf as f64));
    }
    if let Some(status) = filter.status.as_deref()
        && !status.eq_ignore_ascii_case("any")
    {
        clauses.push("status = ?".to_string());
        params.push(Box::new(status.to_string()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    let limit = if filter.limit == 0 { 100 } else { filter.limit };
    sql.push_str(&format!(" ORDER BY rowid DESC LIMIT {limit}"));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(ProposalRow {
            proposal_id: row.get(0)?,
            run_id: row.get(1)?,
            scope: row.get(2)?,
            kind: row.get(3)?,
            text: row.get(4)?,
            rationale: row.get(5)?,
            proposed_confidence: row.get(6)?,
            status: row.get(7)?,
            decided_reason: row.get(8)?,
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
    BrainSession::open(start)?.retrieve_context(stage, query, budget_tokens)
}

pub fn retrieve_context_readonly(
    start: &Path,
    stage: &str,
    query: &str,
    budget_tokens: u32,
) -> KimetsuResult<ContextBundle> {
    BrainSession::open_readonly(start)?.retrieve_context(stage, query, budget_tokens)
}

pub fn retrieve_benchmark_context_readonly(
    start: &Path,
    task: &str,
    dataset: &str,
    task_slug: Option<&str>,
    warm_policy: benchmark::BenchmarkWarmPolicy,
    stage: &str,
    budget_tokens: u32,
    require_benchmark_memory: bool,
    max_capsules: usize,
) -> KimetsuResult<benchmark::BenchmarkBrainContext> {
    let normalized_slug = task_slug
        .and_then(benchmark::normalize_task_slug)
        .or_else(|| benchmark::normalize_task_slug(task));
    let query = benchmark::benchmark_query(task, dataset, normalized_slug.as_deref(), warm_policy);
    let bundle =
        BrainSession::open_readonly(start)?.retrieve_context(stage, &query, budget_tokens)?;
    Ok(benchmark::build_benchmark_context(
        bundle,
        task,
        dataset,
        &query,
        normalized_slug,
        warm_policy,
        require_benchmark_memory,
        max_capsules,
    ))
}

pub fn record_benchmark_outcome(
    start: &Path,
    outcome: benchmark::BenchmarkOutcome,
) -> KimetsuResult<RecordedBenchmarkOutcome> {
    let task_slug = outcome
        .task_slug
        .clone()
        .or_else(|| benchmark::normalize_task_slug(&outcome.task));
    let kind = benchmark::outcome_memory_kind(&outcome);
    let text = benchmark::outcome_memory_text(&outcome);
    let memory_id = add_memory(start, MemoryScope::GlobalUser, kind, &text)?;
    let (proposal_id, proposal_text) = match outcome.generalization.as_ref() {
        Some(proposal) if proposal.role.is_generalizable() => {
            let (proposal_id, proposal_text) = propose_benchmark_memory(start, &outcome, proposal)?;
            (Some(proposal_id), Some(proposal_text))
        }
        _ => (None, None),
    };
    Ok(RecordedBenchmarkOutcome {
        memory_id,
        task_slug,
        kind,
        text,
        proposal_id,
        proposal_text,
    })
}

fn propose_benchmark_memory(
    start: &Path,
    outcome: &benchmark::BenchmarkOutcome,
    proposal: &benchmark::BenchmarkMemoryProposal,
) -> KimetsuResult<(String, String)> {
    let (paths, config, conn) = load_project(start)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "benchmark memory proposal", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;
    let proposal_id = Ulid::new().to_string();
    let text = benchmark::proposal_memory_text(outcome, proposal);
    let kind = benchmark::proposal_memory_kind(proposal);
    let rationale = if proposal.rationale.trim().is_empty() {
        "generalized from benchmark outcome".to_string()
    } else {
        proposal.rationale.trim().to_string()
    };

    let started = admin_started_event(&paths, &config, run_id, "benchmark memory proposal")?;
    writer.append(&started, true)?;

    let proposed = Event::new(
        run_id,
        "memory.proposed",
        serde_json::json!({
            "proposal_id": proposal_id,
            "scope": "global_user",
            "kind": kind.to_string(),
            "text": text,
            "rationale": rationale,
            "proposed_confidence": proposal.confidence.clamp(0.0, 1.0),
            "source_event_ids": [],
        }),
    );
    writer.append(&proposed, true)?;

    let finished = admin_finished_event(run_id);
    writer.append(&finished, true)?;

    projector::apply_events(&conn, &[started, proposed, finished])?;
    Ok((proposal_id, text))
}

pub fn accept_proposal(
    start: &Path,
    proposal_id: &str,
    overrides: AcceptOverrides,
) -> KimetsuResult<String> {
    let (paths, config, conn) = load_project(start)?;
    let proposal = load_pending_proposal(&conn, proposal_id)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory accept", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;
    let memory_id = Ulid::new().to_string();
    let normalized = normalize_memory_text(&proposal.text);

    let resolved_scope = match overrides.scope.as_deref() {
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => proposal.scope.clone(),
    };
    let resolved_confidence = overrides
        .confidence
        .map(|c| c.clamp(0.0, 1.0))
        .unwrap_or(proposal.proposed_confidence);

    let started = admin_started_event(&paths, &config, run_id, "memory accept")?;
    writer.append(&started, true)?;

    let accepted = Event::new(
        run_id,
        "memory.accepted",
        serde_json::json!({
            "proposal_id": proposal.proposal_id,
            "memory_id": memory_id,
            "scope": resolved_scope,
            "kind": proposal.kind,
            "text": proposal.text,
            "normalized_text": normalized,
            "confidence": resolved_confidence,
            "provenance_snapshot": {
                "source": "memory_proposal",
                "proposal_id": proposal.proposal_id,
                "source_run_id": proposal.run_id,
                "scope_override": overrides.scope.clone(),
                "confidence_override": overrides.confidence,
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

/// MP-4d: human override that flags an accepted memory so the broker stops
/// surfacing it. Emits a `memory.invalidated` event and projects it. The
/// canonical trace keeps the original `memory.accepted`; invalidation is
/// purely additive metadata. Idempotent — re-invalidating a memory just
/// overwrites the timestamp/reason.
pub fn invalidate_memory(start: &Path, memory_id: &str, reason: Option<&str>) -> KimetsuResult<()> {
    let (paths, config, conn) = load_project(start)?;
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE memory_id = ?1",
        params![memory_id],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Err(format!("memory not found: {memory_id}").into());
    }

    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory invalidate", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;

    let resolved_reason = reason
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .unwrap_or_else(|| "invalidated_by_cli".to_string());

    let started = admin_started_event(&paths, &config, run_id, "memory invalidate")?;
    writer.append(&started, true)?;

    let invalidated = Event::new(
        run_id,
        "memory.invalidated",
        serde_json::json!({
            "memory_id": memory_id,
            "reason": resolved_reason,
        }),
    );
    writer.append(&invalidated, true)?;

    let finished = admin_finished_event(run_id);
    writer.append(&finished, true)?;

    projector::apply_events(&conn, &[started, invalidated, finished])?;
    Ok(())
}

pub fn reject_proposal(start: &Path, proposal_id: &str, reason: Option<&str>) -> KimetsuResult<()> {
    let (paths, config, conn) = load_project(start)?;
    let _proposal = load_pending_proposal(&conn, proposal_id)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory reject", Some(run_id))?;
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;

    let resolved_reason = reason
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .unwrap_or_else(|| "rejected_by_cli".to_string());

    let started = admin_started_event(&paths, &config, run_id, "memory reject")?;
    writer.append(&started, true)?;

    let rejected = Event::new(
        run_id,
        "memory.rejected",
        serde_json::json!({
            "proposal_id": proposal_id,
            "reason": resolved_reason,
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
        decided_reason: None,
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
    // v0.4.1: pre-v0.4 tests assume `MemoryScope::GlobalUser` writes
    // land in the project DB. With user-brain routing on by default
    // that's no longer true — wrap each affected test in
    // `with_user_brain_disabled` so it sees v0.3.5 routing. Tests
    // that specifically exercise the user-brain path live in
    // `user_brain::tests` and opt-in via `with_user_brain_at`.
    use crate::user_brain::with_user_brain_disabled;

    #[test]
    fn memory_add_survives_projection_rebuild_from_trace() {
        with_user_brain_disabled(|| {
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
        });
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

    #[test]
    fn run_finished_increments_usefulness_for_injected_memories() {
        with_user_brain_disabled(|| {
            // MP-4a outcome attribution: a memory in the context of a run.finished
            // gains +1 usefulness and +1 use_count; per-run counting means the
            // same memory injected into two stages of one run still counts once.
            let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");
            let memory_id = add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Preference,
                "Prefer ripgrep over grep.",
            )
            .expect("add memory");

            {
                let (paths, _config, conn) = load_project(&root).expect("load");
                let run_id = RunId::new();
                let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id).expect("trace");
                let evs: Vec<Event> = vec![
                    Event::new(
                        run_id,
                        "run.started",
                        serde_json::json!({"project_id": "test", "task": "x"}),
                    ),
                    Event::new(
                        run_id,
                        "context.injected",
                        serde_json::json!({
                            "stage": "localization",
                            "capsule_handles": [format!("memory:{memory_id}")],
                            "memory_ids": [memory_id.clone()],
                            "prior_run_ids": [],
                            "file_paths": [],
                        }),
                    ),
                    Event::new(
                        run_id,
                        "context.injected",
                        serde_json::json!({
                            "stage": "patch_plan",
                            "capsule_handles": [format!("memory:{memory_id}")],
                            "memory_ids": [memory_id.clone()],
                            "prior_run_ids": [],
                            "file_paths": [],
                        }),
                    ),
                    Event::new(
                        run_id,
                        "run.finished",
                        serde_json::json!({"status": "success", "total_cost_usd": 0.1}),
                    ),
                ];
                for ev in &evs {
                    writer.append(ev, true).expect("append");
                }
                projector::apply_events(&conn, &evs).expect("project");
            }

            let memories = list_memories(&root).expect("list memories");
            let m = memories.iter().find(|m| m.memory_id == memory_id).unwrap();
            assert_eq!(m.use_count, 1, "per-run counting: 2 stages count once");
            assert!(
                (m.usefulness_score - 1.0).abs() < f32::EPSILON,
                "expected usefulness_score = 1.0, got {}",
                m.usefulness_score
            );

            fs::remove_dir_all(root).expect("remove temp project");
        });
    }

    #[test]
    fn run_failed_decrements_usefulness_unless_gate() {
        // run.failed with category != "Gate" decrements; category == "Gate"
        // is a graceful early-exit (e.g. the plan-create existence guard)
        // and must not blame memories that happened to be in context.
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");
        let memory_id = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "Use find_* for fallible lookups.",
        )
        .expect("add memory");

        {
            let (paths, _config, conn) = load_project(&root).expect("load");

            // First run: gate-failure -> no update at all.
            let gate_run = RunId::new();
            let (mut writer, _) = TraceWriter::create(&paths, gate_run).expect("trace");
            let gate_events: Vec<Event> = vec![
                Event::new(
                    gate_run,
                    "run.started",
                    serde_json::json!({"project_id": "test", "task": "g"}),
                ),
                Event::new(
                    gate_run,
                    "context.injected",
                    serde_json::json!({
                        "stage": "patch_plan",
                        "capsule_handles": [format!("memory:{memory_id}")],
                        "memory_ids": [memory_id.clone()],
                        "prior_run_ids": [],
                        "file_paths": [],
                    }),
                ),
                Event::new(
                    gate_run,
                    "run.failed",
                    serde_json::json!({
                        "category": "Gate",
                        "failed_stage": "patch_plan",
                        "message": "files_to_create_already_exist",
                    }),
                ),
            ];
            for ev in &gate_events {
                writer.append(ev, true).expect("append");
            }
            projector::apply_events(&conn, &gate_events).expect("project gate-fail");

            // Second run: real implementation failure -> -1.
            let impl_run = RunId::new();
            let (mut writer2, _) = TraceWriter::create(&paths, impl_run).expect("trace");
            let impl_events: Vec<Event> = vec![
                Event::new(
                    impl_run,
                    "run.started",
                    serde_json::json!({"project_id": "test", "task": "i"}),
                ),
                Event::new(
                    impl_run,
                    "context.injected",
                    serde_json::json!({
                        "stage": "patch_plan",
                        "capsule_handles": [format!("memory:{memory_id}")],
                        "memory_ids": [memory_id.clone()],
                        "prior_run_ids": [],
                        "file_paths": [],
                    }),
                ),
                Event::new(
                    impl_run,
                    "run.failed",
                    serde_json::json!({
                        "category": "Implementation",
                        "failed_stage": "implementation",
                        "message": "test broke",
                    }),
                ),
            ];
            for ev in &impl_events {
                writer2.append(ev, true).expect("append");
            }
            projector::apply_events(&conn, &impl_events).expect("project impl-fail");
        }

        let memories = list_memories(&root).expect("list memories");
        let m = memories.iter().find(|m| m.memory_id == memory_id).unwrap();
        assert_eq!(m.use_count, 1, "only the non-Gate failure counts as a use");
        assert!(
            (m.usefulness_score - (-1.0)).abs() < f32::EPSILON,
            "expected usefulness_score = -1.0, got {}",
            m.usefulness_score
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    #[test]
    fn run_aborted_does_not_update_usefulness() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");
        let memory_id = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "Module re-exports live in lib.rs.",
        )
        .expect("add memory");

        {
            let (paths, _config, conn) = load_project(&root).expect("load");
            let run_id = RunId::new();
            let (mut writer, _) = TraceWriter::create(&paths, run_id).expect("trace");
            let evs: Vec<Event> = vec![
                Event::new(
                    run_id,
                    "run.started",
                    serde_json::json!({"project_id": "test", "task": "a"}),
                ),
                Event::new(
                    run_id,
                    "context.injected",
                    serde_json::json!({
                        "stage": "patch_plan",
                        "capsule_handles": [format!("memory:{memory_id}")],
                        "memory_ids": [memory_id.clone()],
                        "prior_run_ids": [],
                        "file_paths": [],
                    }),
                ),
                Event::new(
                    run_id,
                    "run.aborted",
                    serde_json::json!({"reason": "user_abort"}),
                ),
            ];
            for ev in &evs {
                writer.append(ev, true).expect("append");
            }
            projector::apply_events(&conn, &evs).expect("project");
        }

        let memories = list_memories(&root).expect("list memories");
        let m = memories.iter().find(|m| m.memory_id == memory_id).unwrap();
        assert_eq!(m.use_count, 0, "aborted runs must not update use_count");
        assert!(
            m.usefulness_score.abs() < f32::EPSILON,
            "expected usefulness_score = 0.0, got {}",
            m.usefulness_score
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    #[test]
    fn list_proposals_filters_and_reject_records_reason() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");

        // Inject three proposals straight via memory.proposed events.
        let proposals = [
            (
                "p1",
                "global_user",
                "preference",
                0.9_f32,
                "Prefer rg over grep",
            ),
            (
                "p2",
                "repo",
                "convention",
                0.8,
                "Use find_* for fallible lookups",
            ),
            (
                "p3",
                "repo",
                "convention",
                0.4,
                "Use let-else where possible",
            ),
        ];
        {
            let (paths, _config, conn) = load_project(&root).expect("load");
            let run_id = RunId::new();
            let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id).expect("trace");
            for (proposal_id, scope, kind, conf, text) in &proposals {
                let event = Event::new(
                    run_id,
                    "memory.proposed",
                    serde_json::json!({
                        "proposal_id": proposal_id,
                        "scope": scope,
                        "kind": kind,
                        "text": text,
                        "rationale": "test rationale",
                        "proposed_confidence": conf,
                        "source_event_ids": [],
                    }),
                );
                writer.append(&event, true).expect("append proposal");
                projector::apply_events(&conn, &[event]).expect("project");
            }
        }

        // Filter by scope.
        let global = list_proposals(
            &root,
            ProposalFilter {
                scope: Some("global_user".into()),
                status: Some("pending".into()),
                ..ProposalFilter::default()
            },
        )
        .expect("list proposals");
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].proposal_id, "p1");

        // Filter by min_confidence.
        let strong = list_proposals(
            &root,
            ProposalFilter {
                min_confidence: Some(0.7),
                status: Some("pending".into()),
                ..ProposalFilter::default()
            },
        )
        .expect("list strong");
        assert_eq!(strong.len(), 2);
        for row in &strong {
            assert!(row.proposed_confidence >= 0.7);
        }

        // Reject one with a reason and confirm it persists on the projected row.
        reject_proposal(&root, "p3", Some("not specific to the user")).expect("reject with reason");
        let rejected = list_proposals(
            &root,
            ProposalFilter {
                status: Some("rejected".into()),
                ..ProposalFilter::default()
            },
        )
        .expect("list rejected");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].proposal_id, "p3");
        assert_eq!(
            rejected[0].decided_reason.as_deref(),
            Some("not specific to the user")
        );

        // Accept with a confidence override and confirm the resulting memory
        // carries the overridden value.
        let memory_id = accept_proposal(
            &root,
            "p1",
            AcceptOverrides {
                scope: None,
                confidence: Some(0.55),
            },
        )
        .expect("accept");
        let memories = list_memories(&root).expect("list memories");
        let promoted = memories
            .into_iter()
            .find(|m| m.memory_id == memory_id)
            .expect("promoted memory present");
        assert!((promoted.confidence - 0.55).abs() < f32::EPSILON);

        fs::remove_dir_all(root).expect("remove temp project");
    }

    /// MP-4d: invalidate_memory emits a `memory.invalidated` event and
    /// projects it. The memory row keeps everything but gains
    /// `invalidated_at`/`invalidated_reason`, and the row survives a
    /// projection rebuild (event is canonical).
    #[test]
    fn invalidate_memory_persists_invalidated_metadata_and_survives_rebuild() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");

        let memory_id = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "Use find_* for fallible lookups.",
        )
        .expect("add memory");

        invalidate_memory(&root, &memory_id, Some("hurt 4 runs in a row"))
            .expect("invalidate memory");

        // Direct DB peek so we can read the new columns even before they are
        // surfaced via MemoryRow.
        {
            let (_paths, _config, conn) = load_project(&root).expect("load");
            let (invalidated_at, invalidated_reason): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT invalidated_at, invalidated_reason FROM memories WHERE memory_id = ?1",
                    params![memory_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("query invalidated metadata");
            assert!(invalidated_at.is_some(), "invalidated_at must be set");
            assert_eq!(invalidated_reason.as_deref(), Some("hurt 4 runs in a row"));
        }

        // Rebuild from trace and confirm invalidation survives.
        rebuild_projection(&root).expect("rebuild projection");
        {
            let (_paths, _config, conn) = load_project(&root).expect("load");
            let invalidated_at: Option<String> = conn
                .query_row(
                    "SELECT invalidated_at FROM memories WHERE memory_id = ?1",
                    params![memory_id],
                    |row| row.get(0),
                )
                .expect("query after rebuild");
            assert!(
                invalidated_at.is_some(),
                "invalidated_at must survive event replay"
            );
        }

        fs::remove_dir_all(root).expect("remove temp project");
    }

    /// MP-4b broker integration: an invalidated memory must not appear in
    /// the retrieved context bundle, even though the row still exists in
    /// brain.db for replay/audit.
    #[test]
    fn invalidated_memory_is_excluded_from_broker_retrieval() {
        with_user_brain_disabled(|| {
            let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            let memory_id = add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Preference,
                "Prefer ripgrep over grep for repo search.",
            )
            .expect("add memory");

            // Sanity: broker surfaces it pre-invalidation.
            let pre = retrieve_context(&root, "localization", "ripgrep grep search", 1200)
                .expect("pre context");
            assert!(
                pre.capsules
                    .iter()
                    .any(|c| c.expansion_handle == format!("memory:{memory_id}")),
                "memory must appear before invalidation: {:?}",
                pre.capsules
            );

            invalidate_memory(&root, &memory_id, Some("no longer accurate")).expect("invalidate");

            let post = retrieve_context(&root, "localization", "ripgrep grep search", 1200)
                .expect("post context");
            assert!(
                post.capsules
                    .iter()
                    .all(|c| c.expansion_handle != format!("memory:{memory_id}")),
                "invalidated memory must not be retrieved: {:?}",
                post.capsules
            );

            // The row itself still exists in brain.db.
            let memories = list_memories(&root).expect("list");
            assert!(memories.iter().any(|m| m.memory_id == memory_id));

            fs::remove_dir_all(root).expect("remove temp project");
        });
    }

    /// MP-6: `list_memories_top` returns invalidated_at IS NULL memories
    /// sorted by ratio descending, filtered by `min_uses`. Memories with
    /// use_count below the threshold are dropped entirely so the listing
    /// only shows entries the broker bias actually applies to.
    #[test]
    fn list_memories_top_sorts_by_usefulness_ratio_and_drops_small_samples() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");

        let m_great =
            add_memory(&root, MemoryScope::Repo, MemoryKind::Convention, "GREAT").expect("great");
        let m_meh =
            add_memory(&root, MemoryScope::Repo, MemoryKind::Convention, "meh").expect("meh");
        let m_bad =
            add_memory(&root, MemoryScope::Repo, MemoryKind::Convention, "BAD").expect("bad");
        let _m_fresh =
            add_memory(&root, MemoryScope::Repo, MemoryKind::Convention, "fresh").expect("fresh");

        // Directly set usefulness data; the event-sourcing path is already
        // tested by `run_finished_increments_usefulness_for_injected_memories`.
        {
            let (_paths, _config, conn) = load_project(&root).expect("load");
            conn.execute(
                "UPDATE memories SET use_count = 5, usefulness_score = 4.0 WHERE memory_id = ?1",
                params![m_great],
            )
            .expect("set great");
            conn.execute(
                "UPDATE memories SET use_count = 5, usefulness_score = 0.0 WHERE memory_id = ?1",
                params![m_meh],
            )
            .expect("set meh");
            conn.execute(
                "UPDATE memories SET use_count = 5, usefulness_score = -3.0 WHERE memory_id = ?1",
                params![m_bad],
            )
            .expect("set bad");
            // m_fresh stays at use_count=0; should be excluded.
        }

        let top = list_memories_top(
            &root,
            TopOptions {
                scope: None,
                min_uses: 3,
                limit: 10,
            },
        )
        .expect("top");
        assert_eq!(top.len(), 3, "fresh memory below min_uses must be excluded");
        assert_eq!(top[0].memory_id, m_great);
        assert_eq!(top[1].memory_id, m_meh);
        assert_eq!(top[2].memory_id, m_bad);

        // Now invalidate the GREAT memory and confirm it disappears.
        invalidate_memory(&root, &m_great, Some("test")).expect("invalidate");
        let top_after = list_memories_top(
            &root,
            TopOptions {
                scope: None,
                min_uses: 3,
                limit: 10,
            },
        )
        .expect("top after");
        assert_eq!(top_after.len(), 2);
        assert!(top_after.iter().all(|m| m.memory_id != m_great));

        fs::remove_dir_all(root).expect("remove temp project");
    }

    /// MP-6: `prune_low_usefulness` lists candidates without writing when
    /// `apply = false`, and invalidates each match via the canonical
    /// `memory.invalidated` event path when `apply = true`. The prune
    /// reason includes the ratio + use_count so audit trail explains
    /// why the memory left.
    #[test]
    fn prune_low_usefulness_dry_run_then_apply() {
        with_user_brain_disabled(|| {
            prune_low_usefulness_dry_run_then_apply_body();
        });
    }

    fn prune_low_usefulness_dry_run_then_apply_body() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");

        let m_keep = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "keep me, I help",
        )
        .expect("keep");
        let m_drop_1 = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "drop me, I hurt",
        )
        .expect("drop1");
        let m_drop_2 = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "drop me too",
        )
        .expect("drop2");
        let m_small_sample = add_memory(
            &root,
            MemoryScope::Repo,
            MemoryKind::Convention,
            "small sample shouldn't be pruned even if score is bad",
        )
        .expect("small");

        {
            let (_paths, _config, conn) = load_project(&root).expect("load");
            // keep: ratio = +0.6 (above threshold)
            conn.execute(
                "UPDATE memories SET use_count = 5, usefulness_score = 3.0 WHERE memory_id = ?1",
                params![m_keep],
            )
            .expect("set keep");
            // drop_1: ratio = -0.6 (well below -0.2)
            conn.execute(
                "UPDATE memories SET use_count = 5, usefulness_score = -3.0 WHERE memory_id = ?1",
                params![m_drop_1],
            )
            .expect("set drop1");
            // drop_2: ratio = -0.4
            conn.execute(
                "UPDATE memories SET use_count = 5, usefulness_score = -2.0 WHERE memory_id = ?1",
                params![m_drop_2],
            )
            .expect("set drop2");
            // small_sample: ratio = -1.0 but only 2 uses, must NOT be pruned
            conn.execute(
                "UPDATE memories SET use_count = 2, usefulness_score = -2.0 WHERE memory_id = ?1",
                params![m_small_sample],
            )
            .expect("set small");
        }

        // Dry-run: lists candidates but does not invalidate.
        let dry = prune_low_usefulness(
            &root,
            PruneOptions {
                scope: None,
                min_uses: 3,
                max_ratio: -0.2,
                apply: false,
            },
        )
        .expect("dry-run");
        assert_eq!(dry.candidates.len(), 2);
        assert_eq!(dry.invalidated, 0);
        let ids: Vec<&str> = dry
            .candidates
            .iter()
            .map(|c| c.memory_id.as_str())
            .collect();
        assert!(ids.contains(&m_drop_1.as_str()));
        assert!(ids.contains(&m_drop_2.as_str()));
        // Confirm small_sample stayed out of the candidate list.
        assert!(!ids.contains(&m_small_sample.as_str()));

        // Pre-apply state: all four memories still active.
        let pre = list_memories(&root).expect("pre");
        assert_eq!(pre.len(), 4);

        // Apply: both bad memories invalidated, keep + small_sample untouched.
        let applied = prune_low_usefulness(
            &root,
            PruneOptions {
                scope: None,
                min_uses: 3,
                max_ratio: -0.2,
                apply: true,
            },
        )
        .expect("apply");
        assert_eq!(applied.candidates.len(), 2);
        assert_eq!(applied.invalidated, 2);
        assert_eq!(applied.failed, 0);

        // Post-apply: list_memories_top with min_uses=3 should now only
        // surface the keep memory (drops are invalidated_at IS NOT NULL,
        // small_sample is filtered by min_uses).
        let top = list_memories_top(
            &root,
            TopOptions {
                scope: None,
                min_uses: 3,
                limit: 10,
            },
        )
        .expect("top after prune");
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].memory_id, m_keep);

        // Confirm the canonical event trail: each pruned memory has a
        // non-null invalidated_at and the reason mentions "pruned_by_usefulness".
        // Scope the connection so it's dropped before fs::remove_dir_all
        // on Windows, where SQLite holds an exclusive lock on the journal.
        {
            let (_paths, _config, conn) = load_project(&root).expect("load");
            let reason: String = conn
                .query_row(
                    "SELECT invalidated_reason FROM memories WHERE memory_id = ?1",
                    params![m_drop_1],
                    |row| row.get(0),
                )
                .expect("invalidated reason");
            assert!(
                reason.starts_with("pruned_by_usefulness"),
                "unexpected reason: {reason}"
            );
        }

        fs::remove_dir_all(root).expect("remove temp project");
    }

    /// MP-5a: the brain primitives behind `kimetsu brain memory review`.
    /// Workflow: inject several proposals across two runs, filter by run +
    /// confidence to pick the keepers, batch-accept those, then
    /// batch-reject the remainder. The final state must show exactly the
    /// accepted proposals as memories and exactly the rejected proposals
    /// carrying a non-empty decided_reason.
    #[test]
    fn batch_review_accepts_filtered_subset_and_rejects_remainder() {
        with_user_brain_disabled(|| {
            batch_review_accepts_filtered_subset_and_rejects_remainder_body();
        });
    }

    fn batch_review_accepts_filtered_subset_and_rejects_remainder_body() {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        fs::create_dir_all(&root).expect("create temp project");
        init_project(&root, false).expect("init project");

        let run_a = RunId::new();
        let run_b = RunId::new();

        // Two proposals from run_a (one strong, one weak) plus two more
        // from run_b. The "review" flow will accept run_a's strong one,
        // reject everything else.
        let proposals: [(&str, RunId, &str, &str, f32, &str); 4] = [
            (
                "p_a_strong",
                run_a,
                "global_user",
                "preference",
                0.92,
                "Prefer rg over grep",
            ),
            (
                "p_a_weak",
                run_a,
                "repo",
                "convention",
                0.55,
                "Always use let-else",
            ),
            (
                "p_b1",
                run_b,
                "repo",
                "convention",
                0.70,
                "Use Result not panic",
            ),
            (
                "p_b2",
                run_b,
                "global_user",
                "preference",
                0.88,
                "Open links in new tab",
            ),
        ];

        {
            let (paths, _config, conn) = load_project(&root).expect("load");
            for (proposal_id, run_id, scope, kind, conf, text) in &proposals {
                let (mut writer, _) = TraceWriter::create(&paths, *run_id).expect("trace");
                let event = Event::new(
                    *run_id,
                    "memory.proposed",
                    serde_json::json!({
                        "proposal_id": proposal_id,
                        "scope": scope,
                        "kind": kind,
                        "text": text,
                        "rationale": "fixture",
                        "proposed_confidence": conf,
                        "source_event_ids": [],
                    }),
                );
                writer.append(&event, true).expect("append");
                projector::apply_events(&conn, &[event]).expect("project");
            }
        }

        // Step 1: --accept-all --from-run <run_a> --min-confidence 0.8
        // mirrors the CLI filter + accept loop.
        let to_accept = list_proposals(
            &root,
            ProposalFilter {
                from_run: Some(run_a.to_string()),
                min_confidence: Some(0.8),
                status: Some("pending".into()),
                limit: 100,
                ..ProposalFilter::default()
            },
        )
        .expect("list strong from run_a");
        assert_eq!(to_accept.len(), 1, "filter should keep only p_a_strong");
        assert_eq!(to_accept[0].proposal_id, "p_a_strong");
        let memory_id =
            accept_proposal(&root, &to_accept[0].proposal_id, AcceptOverrides::default())
                .expect("accept p_a_strong");

        // Step 2: --reject-all --reason "batch_reject" over the remaining
        // pending proposals.
        let to_reject = list_proposals(
            &root,
            ProposalFilter {
                status: Some("pending".into()),
                limit: 100,
                ..ProposalFilter::default()
            },
        )
        .expect("list remaining pending");
        assert_eq!(to_reject.len(), 3, "three proposals should remain pending");
        for p in &to_reject {
            reject_proposal(&root, &p.proposal_id, Some("batch_reject")).expect("reject in batch");
        }

        // Final state: exactly one memory; exactly three rejected proposals;
        // zero pending. Decision reason persisted on each rejected row.
        let memories = list_memories(&root).expect("list memories");
        assert_eq!(
            memories.len(),
            1,
            "only the accepted proposal becomes a memory"
        );
        assert_eq!(memories[0].memory_id, memory_id);

        let pending = list_proposals(
            &root,
            ProposalFilter {
                status: Some("pending".into()),
                limit: 100,
                ..ProposalFilter::default()
            },
        )
        .expect("list pending");
        assert!(
            pending.is_empty(),
            "no proposals left pending after batch review"
        );

        let rejected = list_proposals(
            &root,
            ProposalFilter {
                status: Some("rejected".into()),
                limit: 100,
                ..ProposalFilter::default()
            },
        )
        .expect("list rejected");
        assert_eq!(rejected.len(), 3);
        for row in &rejected {
            assert_eq!(row.decided_reason.as_deref(), Some("batch_reject"));
        }

        fs::remove_dir_all(root).expect("remove temp project");
    }
}
