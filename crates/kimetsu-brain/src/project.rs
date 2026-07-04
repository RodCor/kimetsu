use std::fs;
use std::path::{Path, PathBuf};

use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use kimetsu_core::memory::{MemoryKind, MemoryScope, normalize_memory_text};
use kimetsu_core::paths::{ProjectPaths, default_project_id};
use kimetsu_core::{KIMETSU_CONFIG_VERSION, KimetsuResult};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use ulid::Ulid;

use crate::benchmark;
use crate::conflict;
use crate::context::{self, ContextBundle, ContextRequest};
use crate::embeddings;
use crate::ingest::{self, RepoIngestSummary};
use crate::lock::ProjectLock;
use crate::projector;
use crate::redact;
use crate::schema;
use crate::trace::{self, TraceWriter};
use crate::user_brain;

// ---------------------------------------------------------------------------
// Flagship 2 / Story 2.1: rule-based initial importance estimator
// ---------------------------------------------------------------------------

/// Scan the corpus for the highest cosine similarity to `query_vec`.
/// Returns 0.0 when there are no embeddings or any error occurs.
fn max_corpus_cosine(conn: &Connection, query_vec: &[f32]) -> f32 {
    let mut stmt = match conn.prepare(
        "SELECT embedding FROM memories
         WHERE invalidated_at IS NULL
           AND superseded_by IS NULL
           AND embedding IS NOT NULL
         ORDER BY created_at DESC
         LIMIT 200",
    ) {
        Ok(s) => s,
        Err(_) => return 0.0,
    };
    let rows = match stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) {
        Ok(r) => r,
        Err(_) => return 0.0,
    };
    let mut max_cos: f32 = 0.0;
    for row in rows.flatten() {
        if let Ok(vec) = embeddings::decode_embedding(&row, None) {
            if vec.len() == query_vec.len() {
                let cos = cosine_sim(query_vec, &vec);
                if cos > max_cos {
                    max_cos = cos;
                }
            }
        }
    }
    max_cos
}

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < f32::EPSILON || nb < f32::EPSILON {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

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

/// v0.8: a full-text search hit over memory text, returned by
/// [`search_memories`] and the `kimetsu_brain_memory_search` MCP tool.
/// `rank` is the BM25-derived relevance (higher = more relevant).
#[derive(Debug, Clone)]
pub struct MemorySearchHit {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub rank: f32,
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
    /// v0.8: row offset for paginated navigation from the MCP surface.
    /// 0 = first page (prior behaviour).
    pub offset: u32,
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

// v0.5.1: blame surface — per-run memory attribution. Both the CLI
// (`kimetsu brain memory blame <run-id>`) and the MCP tool
// (`kimetsu_brain_memory_blame`) consume `BlameReport`.

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlameReport {
    pub run_id: String,
    /// Terminal outcome of the run: "success" (run.finished),
    /// "failed" (run.failed), "aborted" (run.aborted), or "unknown"
    /// (no terminal event found yet).
    pub outcome: String,
    /// Failure category when outcome is "failed" (e.g. "Gate",
    /// "Implementation"). None otherwise.
    pub failure_category: Option<String>,
    /// Memories the model explicitly cited via `cite_memory`,
    /// ordered by turn.
    pub cited: Vec<CitedMemory>,
    /// Memories that were retrieved into the run's context but
    /// never cited. They got the weak ±0.1 signal instead of ±1.0.
    pub silent_passengers: Vec<SilentMemory>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CitedMemory {
    pub memory_id: String,
    pub turn: i64,
    pub rationale: Option<String>,
    pub cited_at: String,
    /// Truncated memory text for human-readable output.
    pub text_preview: String,
    pub scope: String,
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SilentMemory {
    pub memory_id: String,
    pub text_preview: String,
    pub scope: String,
    pub kind: String,
}

pub fn init_project(start: &Path, force: bool) -> KimetsuResult<InitSummary> {
    let paths = ProjectPaths::discover(start)?;
    paths.validate_state_dir()?;
    // Create only the `.kimetsu/` dir itself (needed before writing
    // project.toml / brain.db). The `runs/` dir is created lazily by the
    // agent pipeline's TraceWriter — memory writes no longer produce run
    // dirs (W1.4), so a brain-only install never grows a `runs/` tree.
    fs::create_dir_all(&paths.kimetsu_dir)?;

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
    paths.validate_state_dir()?;
    let config = load_config(&paths)?;
    if config.kimetsu.schema_version != KIMETSU_CONFIG_VERSION {
        // Name the offending file: project discovery climbs to the enclosing
        // git root, so the mismatching project.toml is often NOT in the
        // directory the user ran from (e.g. a legacy ~/.kimetsu/project.toml
        // when $HOME is itself a git repo). Without the path this error is a
        // maze — it cost a full benchmark run to locate once.
        return Err(format!(
            "project.toml schema version {} does not match expected {} (file: {}). \
             If this is not the project you meant, run from inside a git \
             repository or pass --workspace to pin the project root.",
            config.kimetsu.schema_version,
            KIMETSU_CONFIG_VERSION,
            paths.project_toml.display()
        )
        .into());
    }

    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;
    Ok((paths, config, conn))
}

/// No-git variant of [`init_project`]: uses [`ProjectPaths::at_root`]
/// directly so discovery never shells out to git or climbs to a parent repo.
/// Intended for the remote HTTP MCP server which manages brains at an
/// explicit root directory per repo-id.
pub fn init_project_at_root(root: &Path, force: bool) -> KimetsuResult<InitSummary> {
    let paths = ProjectPaths::at_root(root);
    paths.validate_state_dir()?;
    fs::create_dir_all(&paths.kimetsu_dir)?;

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

/// No-git variant of [`load_project`]: uses [`ProjectPaths::at_root`]
/// directly so discovery never shells out to git or climbs to a parent repo.
pub fn load_project_at_root(
    root: &Path,
) -> KimetsuResult<(ProjectPaths, ProjectConfig, Connection)> {
    let paths = ProjectPaths::at_root(root);
    paths.validate_state_dir()?;
    let config = load_config(&paths)?;
    if config.kimetsu.schema_version != KIMETSU_CONFIG_VERSION {
        // Name the offending file: project discovery climbs to the enclosing
        // git root, so the mismatching project.toml is often NOT in the
        // directory the user ran from (e.g. a legacy ~/.kimetsu/project.toml
        // when $HOME is itself a git repo). Without the path this error is a
        // maze — it cost a full benchmark run to locate once.
        return Err(format!(
            "project.toml schema version {} does not match expected {} (file: {}). \
             If this is not the project you meant, run from inside a git \
             repository or pass --workspace to pin the project root.",
            config.kimetsu.schema_version,
            KIMETSU_CONFIG_VERSION,
            paths.project_toml.display()
        )
        .into());
    }

    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;
    Ok((paths, config, conn))
}

/// No-git variant of [`load_project_readonly`]: uses [`ProjectPaths::at_root`]
/// directly so discovery never shells out to git or climbs to a parent repo.
pub fn load_project_readonly_at_root(
    root: &Path,
) -> KimetsuResult<(ProjectPaths, ProjectConfig, Connection)> {
    let paths = ProjectPaths::at_root(root);
    paths.validate_state_dir()?;
    let config = load_config(&paths)?;
    if config.kimetsu.schema_version != KIMETSU_CONFIG_VERSION {
        // Name the offending file: project discovery climbs to the enclosing
        // git root, so the mismatching project.toml is often NOT in the
        // directory the user ran from (e.g. a legacy ~/.kimetsu/project.toml
        // when $HOME is itself a git repo). Without the path this error is a
        // maze — it cost a full benchmark run to locate once.
        return Err(format!(
            "project.toml schema version {} does not match expected {} (file: {}). \
             If this is not the project you meant, run from inside a git \
             repository or pass --workspace to pin the project root.",
            config.kimetsu.schema_version,
            KIMETSU_CONFIG_VERSION,
            paths.project_toml.display()
        )
        .into());
    }

    let conn = Connection::open_with_flags(&paths.brain_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    schema::validate(&conn)?;
    Ok((paths, config, conn))
}

/// Return the brain.db schema version for the project rooted at `start`.
///
/// Opens via `load_project` (which migrates on the way through), so by the
/// time this returns the DB is at the current target version.
pub fn schema_version(start: &Path) -> KimetsuResult<i64> {
    let (_, _, conn) = load_project(start)?;
    crate::migrate::current_version(&conn)
}

pub fn load_project_readonly(
    start: &Path,
) -> KimetsuResult<(ProjectPaths, ProjectConfig, Connection)> {
    let paths = ProjectPaths::discover(start)?;
    paths.validate_state_dir()?;
    let config = load_config(&paths)?;
    if config.kimetsu.schema_version != KIMETSU_CONFIG_VERSION {
        // Name the offending file: project discovery climbs to the enclosing
        // git root, so the mismatching project.toml is often NOT in the
        // directory the user ran from (e.g. a legacy ~/.kimetsu/project.toml
        // when $HOME is itself a git repo). Without the path this error is a
        // maze — it cost a full benchmark run to locate once.
        return Err(format!(
            "project.toml schema version {} does not match expected {} (file: {}). \
             If this is not the project you meant, run from inside a git \
             repository or pass --workspace to pin the project root.",
            config.kimetsu.schema_version,
            KIMETSU_CONFIG_VERSION,
            paths.project_toml.display()
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
        // W3.3: honor config.kimetsu.use_user_brain with env override.
        let user_conn = user_brain::open_user_brain_for_config(config.kimetsu.use_user_brain)?;
        Self::from_parts(paths, config, conn, user_conn)
    }

    pub fn open_readonly(start: &Path) -> KimetsuResult<Self> {
        let (paths, config, conn) = load_project_readonly(start)?;
        // Read-only path skips file creation — if the user brain
        // doesn't exist yet we just retrieve from the project DB
        // alone, no surprise file under $HOME.
        // W3.3: honor config.kimetsu.use_user_brain with env override.
        let user_conn =
            user_brain::open_user_brain_readonly_for_config(config.kimetsu.use_user_brain)?;
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
        self.retrieve_context_with_request(ContextRequest {
            stage: stage.to_string(),
            query: query.to_string(),
            budget_tokens,
            ..Default::default()
        })
    }

    /// v0.6: full-request variant used by `kimetsu_brain_context` MCP tool
    /// and `retrieve_context_readonly_with_request` to expose `tags`,
    /// `min_score`, `max_capsules`, and `prefer_roles`.
    ///
    /// W3.1: routes through `open_embedder_for` so the persistent
    /// `[embedder] enabled = false` config field truly disables the
    /// cosine path (FTS-only retrieval) without relying on the env var.
    pub fn retrieve_context_with_request(
        &self,
        mut request: ContextRequest,
    ) -> KimetsuResult<ContextBundle> {
        // v1.0.0: drive the lexical + semantic relevance floors from config
        // unless the caller set its own (non-zero) values.
        if request.min_lexical_coverage == 0.0 {
            request.min_lexical_coverage = self.config.broker.min_lexical_coverage;
        }
        if request.min_semantic_score == 0.0 {
            request.min_semantic_score = self.resolved_min_semantic_score();
        }
        // v2.5: whole-retrieval abstention floor (top direct candidate's score).
        // The gate in context.rs returns an empty bundle when top_score is below
        // this, so a weak retrieval makes the reader abstain. 0.0 = off.
        if request.min_score == 0.0 {
            request.min_score = self.config.broker.abstain_min_score;
        }
        let extras: Vec<&Connection> = self.user_conn.as_ref().into_iter().collect();
        let backend = crate::backend::backend_for(&self.config.storage.backend);
        context::retrieve_context_with_embedder_and_backend(
            &self.conn,
            &self.repo_root,
            &self.config.broker.weights,
            request,
            &extras,
            embeddings::open_embedder_for(self.config.embedder.enabled),
            backend.as_ref(),
        )
    }

    /// v1.0.0: resolve the semantic floor for this session's embedder. The
    /// config default is the AUTO sentinel (-1.0): cosine scales are
    /// MODEL-DEPENDENT — 0.35 suits bge-family distributions, but the remote
    /// benchmark showed the same floor killing relevant jina-v2 results
    /// outright (MRR 0.90 → 0.77, recall@2 == recall@4) — so auto applies
    /// the bge-calibrated floor only to bge models and disables it
    /// elsewhere (jina-v2's own precision keeps noise low without it).
    /// Explicit non-negative config values are used as-is for any model.
    fn resolved_min_semantic_score(&self) -> f32 {
        let configured = self.config.broker.min_semantic_score;
        if configured >= 0.0 {
            return configured;
        }
        let model = embeddings::resolve_embedder_id(Some(self.config.embedder.model.as_str()));
        if model.starts_with("bge") { 0.35 } else { 0.0 }
    }

    /// v0.8: proactive (mid-work) retrieval. Pins [`NoopEmbedder`] so
    /// it stays lexical-FTS-only — NO embedding model is loaded even in
    /// `--features embeddings` builds, keeping the per-tool-call hook
    /// cheap. `request.kinds` should restrict to actionable kinds; the
    /// caller sets a high `min_score` and `max_capsules: 1` so recall is
    /// rare and confident (the human-brain "it comes to you" model).
    pub fn retrieve_proactive(&self, mut request: ContextRequest) -> KimetsuResult<ContextBundle> {
        // v1.0.0: the lexical floor applies here too — proactive recall is
        // FTS-only, so without it an off-topic memory sharing a ubiquitous
        // token with the command line (e.g. "config") can take the single
        // proactive slot.
        if request.min_lexical_coverage == 0.0 {
            request.min_lexical_coverage = self.config.broker.min_lexical_coverage;
        }
        let extras: Vec<&Connection> = self.user_conn.as_ref().into_iter().collect();
        let backend = crate::backend::backend_for(&self.config.storage.backend);
        context::retrieve_context_with_embedder_and_backend(
            &self.conn,
            &self.repo_root,
            &self.config.broker.weights,
            request,
            &extras,
            &embeddings::NoopEmbedder,
            backend.as_ref(),
        )
    }

    /// v1.0.0: lexical (FTS-only) retrieval honoring the full
    /// [`ContextRequest`]. Like [`Self::retrieve_context_with_request`]
    /// but pins [`NoopEmbedder`] so NO embedding model is loaded even in
    /// `--features embeddings` builds. The `UserPromptSubmit` context-hook
    /// uses this: it runs in a throwaway per-prompt process that cannot
    /// reuse the long-lived MCP server's warm model cache, so a cold ONNX
    /// load there can blow the host's 30s hook timeout. Semantic ANN
    /// recall stays with the warm MCP `kimetsu_brain_context` tool.
    pub fn retrieve_context_lexical(
        &self,
        mut request: ContextRequest,
    ) -> KimetsuResult<ContextBundle> {
        // v1.0.0: the hook path is FTS-only, so this lexical floor (driven
        // from config unless the caller overrode it) is the *only* relevance
        // gate protecting it — the cosine-based `min_semantic_score` is inert
        // here.
        if request.min_lexical_coverage == 0.0 {
            request.min_lexical_coverage = self.config.broker.min_lexical_coverage;
        }
        let extras: Vec<&Connection> = self.user_conn.as_ref().into_iter().collect();
        let backend = crate::backend::backend_for(&self.config.storage.backend);
        context::retrieve_context_with_embedder_and_backend(
            &self.conn,
            &self.repo_root,
            &self.config.broker.weights,
            request,
            &extras,
            &embeddings::NoopEmbedder,
            backend.as_ref(),
        )
    }

    /// v1.0.0: read-only retrieval honoring the full [`ContextRequest`] but
    /// with a caller-supplied embedder. The warm embedder daemon uses this
    /// to run cosine/ANN retrieval with ONE long-lived model instead of
    /// opening a fresh embedder per request. The lexical-coverage floor is
    /// applied here too (driven from config unless the caller overrode it).
    pub fn retrieve_context_with_injected_embedder(
        &self,
        mut request: ContextRequest,
        embedder: &dyn embeddings::Embedder,
    ) -> KimetsuResult<ContextBundle> {
        if request.min_lexical_coverage == 0.0 {
            request.min_lexical_coverage = self.config.broker.min_lexical_coverage;
        }
        // v1.0.0: semantic floor from config too — this is the daemon's path,
        // where a real query embedding makes the cosine floor effective.
        if request.min_semantic_score == 0.0 {
            request.min_semantic_score = self.resolved_min_semantic_score();
        }
        // v2.5: whole-retrieval abstention floor (top direct candidate's score).
        // The gate in context.rs returns an empty bundle when top_score is below
        // this, so a weak retrieval makes the reader abstain. 0.0 = off.
        if request.min_score == 0.0 {
            request.min_score = self.config.broker.abstain_min_score;
        }
        let extras: Vec<&Connection> = self.user_conn.as_ref().into_iter().collect();
        let backend = crate::backend::backend_for(&self.config.storage.backend);
        context::retrieve_context_with_embedder_and_backend(
            &self.conn,
            &self.repo_root,
            &self.config.broker.weights,
            request,
            &extras,
            embedder,
            backend.as_ref(),
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
    let mut config = ProjectConfig::from_toml(&content)?;
    // Resolve the [retrieval] level preset into [embedder].enabled +
    // [embedder].reranker BEFORE returning, so every retrieval consumer
    // (the config.embedder.enabled sites + the daemon reranker resolution)
    // sees the resolved values automatically. "custom" (the default) is a
    // no-op, so configs without [retrieval] are byte-identical in behaviour.
    config.apply_retrieval_level();
    Ok(config)
}

/// D2: Parse a project config from raw TOML text. Used by `config edit`
/// to validate the file the user just saved before confirming success.
pub fn load_config_from_text(toml: &str) -> KimetsuResult<ProjectConfig> {
    ProjectConfig::from_toml(toml)
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

/// One entry in a [`add_memories_batch`] call.
///
/// `text` is required; all other fields are optional and fall back to
/// the defaults documented on each field.
#[derive(Debug, Clone)]
pub struct BatchMemoryEntry {
    /// The memory text to store.
    pub text: String,
    /// Scope to store under.  Defaults to `MemoryScope::Project`.
    pub scope: MemoryScope,
    /// Memory kind.  Defaults to `MemoryKind::Fact`.
    pub kind: MemoryKind,
    /// Flagship 1 / temporal: optional RFC 3339 valid-from bound.
    /// `None` leaves the column NULL (valid forever from creation).
    pub valid_from: Option<String>,
    /// Flagship 1 / temporal: optional RFC 3339 valid-to bound.
    /// `None` leaves the column NULL (no expiry).
    pub valid_to: Option<String>,
}

/// Initial confidence for a directly-added memory (`memory add` / `add-batch`).
///
/// Story 2.4 follow-up: a freshly written memory is asserted but UNPROVEN, so it
/// must not start at the 1.0 ceiling. The outcome-update path nudges confidence
/// toward 1.0 on citation (asymptoting to the 0.99 clamp) and toward 0.0 on
/// regret, so a default below the clamp leaves headroom for a proven memory to
/// outrank a never-evaluated one — instead of every fresh memory pinning at the
/// top. All directly-added memories share this value, so retrieval ranking and
/// contradiction resolution (which compare confidence) are unchanged for the
/// uniform case; the value only matters once outcomes differentiate memories.
const DIRECT_ADD_CONFIDENCE: f32 = 0.85;

pub fn add_memory(
    start: &Path,
    scope: MemoryScope,
    kind: MemoryKind,
    text: &str,
) -> KimetsuResult<String> {
    // v0.4.5: redact secrets at the ingest boundary. The redaction
    // pipeline catches Anthropic/OpenAI/GitHub/AWS/Slack/Google
    // credentials, JWTs, PEM blocks, and generic `api_key=...` /
    // `bearer ...` / `token: ...` assignments. A leak that lands in
    // brain.db is durable, replicated across user / project scopes,
    // and shows up in every retrieval — better to false-positive on
    // a config string than to leak a real key.
    //
    // On a hit we replace the bytes with `[REDACTED:<kind>]` and
    // print a one-liner to stderr so the operator notices. We do
    // NOT fail the write: keeping the user memorable (the rest of
    // the text) is more useful than rejecting outright.
    let redaction = redact::redact_secrets(text);
    if redaction.was_redacted() {
        eprintln!("kimetsu-brain: {}", redaction.summary());
    }
    let text = redaction.text.as_str();

    // v0.4.1: GlobalUser memories route to `~/.kimetsu/brain.db` when
    // the user brain is enabled. The user-brain write path is
    // intentionally simpler (no run rows, no trace events, no project
    // lock) because there's no project to attribute them to.
    //
    // If the user brain is disabled (KIMETSU_USER_BRAIN=0 or
    // config.kimetsu.use_user_brain=false) OR unreachable (no $HOME),
    // fall through to the project DB so backward compat is preserved —
    // existing scripts that wrote GlobalUser memories into the project
    // keep working.
    //
    // P0 fix: this short-circuit MUST run BEFORE `load_project` so
    // that GlobalUser writes work from ANY `start` directory — including
    // dirs that are not kimetsu projects (e.g. the global distiller's
    // temp/user dir). W3.3's `use_user_brain` toggle is still honored
    // best-effort: if `start` IS a project we read its config; if not
    // (or if the read fails) we default to enabled (nothing to opt out of).
    if scope == MemoryScope::GlobalUser {
        let use_user_brain = ProjectPaths::discover(start)
            .ok()
            .and_then(|paths| load_config(&paths).ok())
            .map(|cfg| cfg.kimetsu.use_user_brain)
            .unwrap_or(true);
        if let Some(user_conn) = user_brain::open_user_brain_for_config(use_user_brain)? {
            return user_brain::add_user_memory(&user_conn, kind, text, 1.0);
        }
        // User brain disabled/unreachable → fall through to the project DB
        // (which DOES require a valid project — same pre-P0 behavior for
        // the disabled/fallback path).
    }

    let (paths, config, conn) = load_project(start)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory add", Some(run_id))?;

    let embedder = embeddings::open_embedder_for(config.embedder.enabled);
    add_memory_inner(
        &conn, &paths, &config, scope, kind, text, None, None, embedder,
    )
}

/// Per-entry core shared by [`add_memory`] and [`add_memories_batch`].
///
/// Takes an already-open connection + loaded config + resolved embedder so
/// neither the project nor the embedder is re-initialized per call.
/// The single-add path acquires the project lock once before calling this;
/// the batch path acquires it once for the whole batch.
///
/// Returns the `memory_id` of the written (or deduped) memory.
#[allow(clippy::too_many_arguments)]
fn add_memory_inner(
    conn: &Connection,
    paths: &ProjectPaths,
    config: &kimetsu_core::config::ProjectConfig,
    scope: MemoryScope,
    kind: MemoryKind,
    text: &str,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
    embedder: &dyn embeddings::Embedder,
) -> KimetsuResult<String> {
    let run_id = RunId::new();
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
              AND superseded_by IS NULL
            LIMIT 1
            ",
            rusqlite::params![scope.to_string(), kind.to_string(), normalized],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing_id) = existing {
        return Ok(existing_id);
    }

    // Flagship 2 / Story 2.1: compute kind-weight portion of initial
    // usefulness BEFORE writing the event so the value is in the event
    // payload (rebuild-safe).  Rarity bonus (requires embedding) is applied
    // as a follow-up UPDATE after embed_and_persist — not in the event, so it
    // degrades to 0 on rebuild, but that is acceptable for a bootstrap seed.
    let importance_enabled = config.ingestion.initial_importance_scoring;
    let initial_kind_weight = if importance_enabled {
        match &kind {
            MemoryKind::FailurePattern => 0.3_f32,
            MemoryKind::Command => 0.2,
            MemoryKind::Convention => 0.15,
            MemoryKind::Fact => 0.1,
            MemoryKind::Preference => 0.05,
        }
    } else {
        0.0
    };

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
            "confidence": DIRECT_ADD_CONFIDENCE,
            "initial_usefulness": initial_kind_weight,
            "provenance_snapshot": build_provenance(run_id, text),
        }),
    );

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

    projector::apply_events(conn, &[started, accepted, finished])?;

    // Flagship 1 / temporal: stamp valid_from / valid_to when requested.
    // This is event-sourced (rebuild-safe) via mark_memory_temporal.
    if valid_from.is_some() || valid_to.is_some() {
        projector::mark_memory_temporal(conn, &memory_id, valid_from, valid_to)?;
    }

    // v0.4.2: post-projection embedding write. v0.4.3 wired the
    // default embedder behind a feature flag — see
    // `embeddings::open_default_embedder`. Default build: NoopEmbedder
    // (column stays NULL, FTS only). `--features embeddings` build:
    // fastembed-rs BGE-small by default, configurable via
    // KIMETSU_BRAIN_EMBEDDER. The embedder is cached in a
    // process-static OnceLock so we only pay model-load cost once.
    // W3.1: route through open_embedder_for so `[embedder] enabled = false`
    // in project.toml durably disables vector writes (FTS-only).
    //
    // embed_and_persist returns the computed vector so we can reuse it for
    // conflict detection without re-embedding (Fix 4c — halves embedding cost).
    let embedding_vec = embeddings::embed_and_persist(conn, &memory_id, text, embedder)?;

    // Flagship 2 / Story 2.1: apply rarity bonus (requires embedding).
    // The kind-weight was already stored in the event; now compute the rarity
    // bonus (if embedder is active and we got a vector) and UPDATE the row.
    // This is NOT rebuild-safe (rarity depends on the corpus snapshot at write
    // time), which is acceptable: on rebuild, the kind-weight from the event
    // is used and the rarity bonus is 0.
    if importance_enabled && !embedder.is_noop() {
        if let Some(vec) = embedding_vec.as_deref() {
            let rarity_bonus = {
                let max_cos = max_corpus_cosine(conn, vec);
                if max_cos < 0.5 { 0.1_f32 } else { 0.0 }
            };
            if rarity_bonus > 0.0 {
                let full_score = (initial_kind_weight + rarity_bonus).min(0.5);
                conn.execute(
                    "UPDATE memories SET usefulness_score = ?2 WHERE memory_id = ?1",
                    rusqlite::params![memory_id, full_score],
                )
                .ok(); // best-effort
            }
        }
    }

    // v0.5.2 / v1.0: conflict detection at ingest. Scans for high-cosine,
    // different-text neighbors in the same scope and logs each pair
    // to `memory_conflicts` for operator review via
    // `kimetsu brain memory conflicts`. Best-effort: NoopEmbedder
    // (lean build) returns 0 hits; embedder failures degrade to a
    // stderr line, never to a failed insert.
    //
    // v1.0: honor the [ingestion] detect_conflicts config field and the
    // KIMETSU_DETECT_CONFLICTS env override so bulk-seeding can skip the
    // O(N²) conflict scan.
    //
    // v2.5 Pass B (Story 1.3): when resolve_conflicts is also enabled, run
    // auto-resolution: clear winners (confidence×recency gap ≥ 0.15) have
    // the loser's valid_to stamped; near-ties go to the queue.
    if conflict::conflict_detection_enabled(config.ingestion.detect_conflicts) {
        // Fetch the created_at timestamp of the newly-written memory for
        // scoring (needed by resolve_conflicts).  We read it back from the DB
        // because the event timestamp is the canonical value.
        let new_created_at: String = conn
            .query_row(
                "SELECT created_at FROM memories WHERE memory_id = ?1",
                rusqlite::params![memory_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| {
                // Fallback: use "now" so recency scoring is still valid.
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            });

        if conflict::resolve_conflicts_enabled(config.ingestion.resolve_conflicts) {
            // Pass B: detect + auto-resolve.
            let (auto_resolved, queued) = conflict::detect_record_and_resolve_with_vec(
                conn,
                &memory_id,
                &scope,
                &kind.to_string(),
                text,
                embedding_vec.as_deref(),
                embedder,
                DIRECT_ADD_CONFIDENCE, // matches the memory.accepted event above
                &new_created_at,
            );
            if auto_resolved > 0 {
                eprintln!(
                    "kimetsu-brain: memory {memory_id} auto-resolved {auto_resolved} contradiction{} (loser valid_to stamped)",
                    if auto_resolved == 1 { "" } else { "s" }
                );
            }
            if queued > 0 {
                eprintln!(
                    "kimetsu-brain: memory {memory_id} has {queued} near-tie conflict{} queued for review (run `kimetsu brain memory conflicts`)",
                    if queued == 1 { "" } else { "s" }
                );
            }
        } else {
            // Detect-only (Pass A / disabled-resolution) path.
            let conflicts = conflict::detect_and_record_with_vec(
                conn,
                &memory_id,
                &scope,
                &kind.to_string(),
                text,
                embedding_vec.as_deref(),
                embedder,
            );
            if conflicts > 0 {
                eprintln!(
                    "kimetsu-brain: memory {memory_id} conflicts with {conflicts} existing memor{} (run `kimetsu brain memory conflicts` to review)",
                    if conflicts == 1 { "y" } else { "ies" }
                );
            }
        }
    }

    Ok(memory_id)
}

/// Add many memories in one process: the project is opened and the embedder
/// is initialized ONCE, then every entry is processed by [`add_memory_inner`].
///
/// This is the efficient ingest path for benchmarks (LongMemEval etc.) and
/// bulk imports: the per-call overhead of `load_project` + embedder init is
/// paid exactly once regardless of how many entries are in `entries`.
///
/// # Behaviour
/// * Entries whose `scope` is `GlobalUser` are silently routed to the user
///   brain (when enabled), exactly as the single-add path does.
/// * Dedup, redaction, conflict detection, rarity scoring, and temporal
///   stamping all apply per-entry — identical to the single-add path.
/// * Returns `Vec<String>` of memory IDs in the same order as `entries`.
///   Deduped entries return the existing memory ID (not an error).
///
/// # Errors
/// The function opens the project once; if `load_project` fails the error is
/// returned before any entries are processed. Per-entry failures propagate
/// immediately (fail-fast), leaving already-written entries in the DB.
pub fn add_memories_batch(
    start: &Path,
    entries: Vec<BatchMemoryEntry>,
) -> KimetsuResult<Vec<String>> {
    if entries.is_empty() {
        return Ok(vec![]);
    }

    // Determine user-brain config (needed for GlobalUser routing) without
    // requiring a valid project — same best-effort approach as single-add.
    let use_user_brain = ProjectPaths::discover(start)
        .ok()
        .and_then(|paths| load_config(&paths).ok())
        .map(|cfg| cfg.kimetsu.use_user_brain)
        .unwrap_or(true);

    // Open user brain once (if available) so GlobalUser entries share it.
    let user_conn_opt = user_brain::open_user_brain_for_config(use_user_brain)?;

    // Check whether any non-GlobalUser entries exist; only open the project
    // if needed (avoids failing on user-only batches in non-project dirs).
    let has_project_entries = entries.iter().any(|e| e.scope != MemoryScope::GlobalUser);

    // Open project + embedder ONCE for all project-scoped entries.
    let project_state: Option<(
        ProjectPaths,
        kimetsu_core::config::ProjectConfig,
        Connection,
    )> = if has_project_entries {
        let state = load_project(start)?;
        Some(state)
    } else {
        None
    };

    // Acquire project lock once for the whole batch (if we have a project).
    let run_id_for_lock = RunId::new();
    let _lock = if let Some((ref paths, _, _)) = project_state {
        Some(ProjectLock::acquire(
            paths,
            "brain memory add-batch",
            Some(run_id_for_lock),
        )?)
    } else {
        None
    };

    // Resolve embedder once — the key perf benefit: model loaded once,
    // not once per entry.
    let embedder: &dyn embeddings::Embedder = if let Some((_, ref config, _)) = project_state {
        embeddings::open_embedder_for(config.embedder.enabled)
    } else {
        &embeddings::NoopEmbedder
    };

    let mut ids = Vec::with_capacity(entries.len());

    for entry in entries {
        // Redact at the ingest boundary (same as single-add).
        let redaction = redact::redact_secrets(&entry.text);
        if redaction.was_redacted() {
            eprintln!("kimetsu-brain: {}", redaction.summary());
        }
        let text = redaction.text.as_str();

        if entry.scope == MemoryScope::GlobalUser {
            // Route to user brain when available; otherwise fall through to
            // project DB — same behaviour as the single-add path.
            if let Some(ref uc) = user_conn_opt {
                let id = user_brain::add_user_memory(uc, entry.kind, text, 1.0)?;
                ids.push(id);
                continue;
            }
            // Fall through: user brain disabled/unreachable, write to project.
        }

        let (paths, config, conn) = project_state
            .as_ref()
            .expect("project must be open when non-GlobalUser entries are present");

        let id = add_memory_inner(
            conn,
            paths,
            config,
            entry.scope,
            entry.kind,
            text,
            entry.valid_from.as_deref(),
            entry.valid_to.as_deref(),
            embedder,
        )?;
        ids.push(id);
    }

    Ok(ids)
}

/// v0.6: write a `memory.proposed` event (pending proposal) without
/// accepting it immediately. Used by `kimetsu_brain_record` when confidence
/// is low and the lesson needs human review before entering the retrieval pool.
/// Returns the `proposal_id`.
pub fn propose_memory(
    start: &Path,
    scope: MemoryScope,
    kind: MemoryKind,
    text: &str,
    confidence: f32,
    rationale: &str,
) -> KimetsuResult<String> {
    let redaction = redact::redact_secrets(text);
    if redaction.was_redacted() {
        eprintln!("kimetsu-brain: {}", redaction.summary());
    }
    let text = redaction.text.as_str();
    let rationale_redaction = redact::redact_secrets(rationale);
    if rationale_redaction.was_redacted() {
        eprintln!("kimetsu-brain: {}", rationale_redaction.summary());
    }
    let rationale = rationale_redaction.text.as_str();
    let (paths, config, conn) = load_project(start)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "memory propose", Some(run_id))?;
    let proposal_id = Ulid::new().to_string();

    let started = admin_started_event(&paths, &config, run_id, "memory propose")?;

    let proposed = Event::new(
        run_id,
        "memory.proposed",
        serde_json::json!({
            "proposal_id": proposal_id,
            "scope": scope.to_string(),
            "kind": kind.to_string(),
            "text": text,
            "rationale": rationale,
            "proposed_confidence": confidence.clamp(0.0, 1.0),
            "source_event_ids": [],
        }),
    );

    let finished = admin_finished_event(run_id);

    projector::apply_events(&conn, &[started, proposed, finished])?;
    Ok(proposal_id)
}

/// v0.7: outcome of a `propose_or_merge_memory` call.
#[derive(Debug)]
pub enum ProposeResult {
    Added(String),     // memory_id — new memory, directly accepted
    Proposed(String),  // proposal_id — pending for review (low confidence)
    Merged(String),    // memory_id of the existing memory that was updated
    Duplicate(String), // memory_id of the identical existing memory
}

/// v0.7: capture a lesson, automatically deduplicating against the existing brain.
///
/// Decision tree:
/// 1. Exact normalized-text match → `Duplicate` (no write).
/// 2. Cosine similarity ≥ 0.85 with an existing memory → `Merged` (append & re-embed).
/// 3. confidence ≥ 0.7 and no close match → `Added` (direct acceptance).
/// 4. confidence < 0.7 → `Proposed` (pending for human review).
///
/// Step 2 only fires when the embedder is active (bge-small or similar). In lean builds
/// the cosine scan returns nothing and the function falls through to step 3/4.
pub fn propose_or_merge_memory(
    start: &Path,
    scope: MemoryScope,
    kind: MemoryKind,
    text: &str,
    confidence: f32,
    rationale: &str,
) -> KimetsuResult<ProposeResult> {
    let redaction = redact::redact_secrets(text);
    if redaction.was_redacted() {
        eprintln!("kimetsu-brain: {}", redaction.summary());
    }
    let text = redaction.text.as_str();

    // Step 1: exact normalized-text dedup (same as add_memory).
    // W3.1: load config here so Step 2 can use open_embedder_for.
    let (_, config, _) = {
        let (paths, config, ro_conn) = load_project_readonly(start)?;
        let normalized = normalize_memory_text(text);
        let existing: Option<String> = ro_conn
            .query_row(
                "SELECT memory_id FROM memories
                 WHERE scope = ?1 AND kind = ?2 AND normalized_text = ?3
                   AND invalidated_at IS NULL
                   AND superseded_by IS NULL
                 LIMIT 1",
                rusqlite::params![scope.to_string(), kind.to_string(), normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(ProposeResult::Duplicate(id));
        }
        (paths, config, ro_conn)
    };

    // Step 2: semantic dedup — look for a high-cosine existing memory.
    // W3.1: route through open_embedder_for so `[embedder] enabled = false`
    // skips cosine dedup (NoopEmbedder → find_potential_conflicts returns 0).
    // v1.0: honor the [ingestion] detect_conflicts off-switch so bulk-seeding
    // skips the cosine scan (find_potential_conflicts returns empty → no merge).
    let embedder = embeddings::open_embedder_for(config.embedder.enabled);
    {
        let (_, _, ro_conn) = load_project_readonly(start)?;
        let conflicts = if conflict::conflict_detection_enabled(config.ingestion.detect_conflicts) {
            conflict::find_potential_conflicts(&ro_conn, &scope, text, embedder, 1, 0.85)?
        } else {
            Vec::new()
        };
        if let Some(hit) = conflicts.into_iter().next() {
            // Append the new lesson to the existing memory and re-embed it.
            let (paths, _config, conn) = load_project(start)?;
            let run_id = RunId::new();
            let _lock = ProjectLock::acquire(&paths, "memory merge", Some(run_id))?;
            let merged_text = format!("{}\n\nAlso: {text}", hit.existing_text);
            let new_normalized = normalize_memory_text(&merged_text);
            conn.execute(
                "UPDATE memories
                 SET text = ?1, normalized_text = ?2, use_count = use_count + 1
                 WHERE memory_id = ?3",
                rusqlite::params![merged_text, new_normalized, hit.existing_memory_id],
            )?;
            // Return value not needed — no conflict scan after a merge.
            embeddings::embed_and_persist(&conn, &hit.existing_memory_id, &merged_text, embedder)?;
            return Ok(ProposeResult::Merged(hit.existing_memory_id));
        }
    }

    // Step 3/4: no close match found — accept or propose based on confidence.
    if confidence >= 0.7 {
        let memory_id = add_memory(start, scope, kind, text)?;
        Ok(ProposeResult::Added(memory_id))
    } else {
        let proposal_id = propose_memory(start, scope, kind, text, confidence, rationale)?;
        Ok(ProposeResult::Proposed(proposal_id))
    }
}

/// v0.8: pagination + scope filter for `list_memories_with`, surfaced
/// by the `kimetsu_brain_memory_list` MCP tool so an agent can page
/// through the corpus from inside Claude/Codex.
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// Max project rows to return. 0 → 100 (the prior default).
    pub limit: u32,
    /// Project-row offset (for paging). 0 → first page.
    pub offset: u32,
    /// Optional scope filter (global_user / project / repo / run).
    pub scope: Option<String>,
}

impl Default for ListOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            offset: 0,
            scope: None,
        }
    }
}

pub fn list_memories(start: &Path) -> KimetsuResult<Vec<MemoryRow>> {
    list_memories_with(start, ListOptions::default())
}

/// v0.8: paginated/scoped memory listing. The project page is bounded
/// by `limit`/`offset`; the user brain's portable rows are appended
/// only on the first page (`offset == 0`) so they appear exactly once
/// during navigation rather than on every page.
pub fn list_memories_with(start: &Path, opts: ListOptions) -> KimetsuResult<Vec<MemoryRow>> {
    let (_paths, config, conn) = load_project(start)?;
    let mut memories = list_memories_from_conn(&conn, &opts)?;
    // W3.3: honor config.kimetsu.use_user_brain with env override.
    if opts.offset == 0
        && let Some(user_conn) =
            user_brain::open_user_brain_readonly_for_config(config.kimetsu.use_user_brain)?
    {
        memories.extend(user_brain::list_user_memories(&user_conn)?);
    }
    Ok(memories)
}

/// v0.5.1: per-run memory attribution. Walks `memory_citations`,
/// the run's `context.injected` events, and (when present) the
/// terminal run.finished/failed/aborted event to produce a
/// `BlameReport` that surfaces which memories the model actually
/// reasoned with vs which were silent passengers.
///
/// Lookups across user + project brains are merged so a cited
/// user-scope memory shows its text even when the run lived in a
/// project brain.
pub fn blame_run(start: &Path, run_id: &str) -> KimetsuResult<BlameReport> {
    let (_paths, config, conn) = load_project(start)?;
    // W3.3: honor config.kimetsu.use_user_brain with env override.
    let user_conn = user_brain::open_user_brain_readonly_for_config(config.kimetsu.use_user_brain)?;

    // 1. Terminal outcome.
    let (outcome, failure_category) = run_outcome(&conn, run_id)?;

    // 2. Cited memories — ordered by turn.
    let cited_rows: Vec<(String, i64, Option<String>, String)> = {
        let mut stmt = conn.prepare(
            "
            SELECT memory_id, turn, rationale, cited_at
            FROM memory_citations
            WHERE run_id = ?1
            ORDER BY turn ASC, cited_at ASC
            ",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out
    };

    let mut cited: Vec<CitedMemory> = Vec::with_capacity(cited_rows.len());
    let mut cited_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (memory_id, turn, rationale, cited_at) in cited_rows {
        cited_set.insert(memory_id.clone());
        let (text, scope, kind) = resolve_memory(&conn, user_conn.as_ref(), &memory_id);
        cited.push(CitedMemory {
            memory_id,
            turn,
            rationale,
            cited_at,
            text_preview: text_preview(&text, 120),
            scope,
            kind,
        });
    }

    // 3. Silent passengers — retrieved but not cited.
    let retrieved_ids = collect_injected_memory_ids_for_blame(&conn, run_id)?;
    let mut silent: Vec<SilentMemory> = Vec::new();
    for memory_id in retrieved_ids {
        if cited_set.contains(&memory_id) {
            continue;
        }
        let (text, scope, kind) = resolve_memory(&conn, user_conn.as_ref(), &memory_id);
        silent.push(SilentMemory {
            memory_id,
            text_preview: text_preview(&text, 120),
            scope,
            kind,
        });
    }

    Ok(BlameReport {
        run_id: run_id.to_string(),
        outcome,
        failure_category,
        cited,
        silent_passengers: silent,
    })
}

fn run_outcome(conn: &Connection, run_id: &str) -> KimetsuResult<(String, Option<String>)> {
    // Pull the most recent terminal event for the run, if any.
    let row: Option<(String, String)> = conn
        .query_row(
            "
            SELECT kind, payload_json
            FROM events
            WHERE run_id = ?1
              AND kind IN ('run.finished', 'run.failed', 'run.aborted')
            ORDER BY ts DESC
            LIMIT 1
            ",
            rusqlite::params![run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((kind, payload_json)) => {
            let outcome = match kind.as_str() {
                "run.finished" => "success".to_string(),
                "run.failed" => "failed".to_string(),
                "run.aborted" => "aborted".to_string(),
                other => other.to_string(),
            };
            let category = if kind == "run.failed" {
                serde_json::from_str::<serde_json::Value>(&payload_json)
                    .ok()
                    .and_then(|v| {
                        v.get("category")
                            .and_then(|c| c.as_str())
                            .map(str::to_string)
                    })
            } else {
                None
            };
            (outcome, category)
        }
        None => ("unknown".to_string(), None),
    })
}

fn collect_injected_memory_ids_for_blame(
    conn: &Connection,
    run_id: &str,
) -> KimetsuResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "
        SELECT payload_json
        FROM events
        WHERE run_id = ?1 AND kind = 'context.injected'
        ",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| row.get::<_, String>(0))?;
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let payload_json = row?;
        let payload: serde_json::Value = serde_json::from_str(&payload_json)?;
        if let Some(ids) = payload.get("memory_ids").and_then(|v| v.as_array()) {
            for id in ids {
                if let Some(s) = id.as_str()
                    && !s.is_empty()
                {
                    seen.insert(s.to_string());
                }
            }
        }
    }
    Ok(seen.into_iter().collect())
}

/// Look up a memory's (text, scope, kind) across the project conn
/// and the optional user-brain conn. Returns
/// ("<unknown — deleted?>", "", "") when the row isn't found in
/// either DB (e.g. invalidated + GC'd, or a typo'd memory_id in
/// the citation).
fn resolve_memory(
    project_conn: &Connection,
    user_conn: Option<&Connection>,
    memory_id: &str,
) -> (String, String, String) {
    let q = "SELECT text, scope, kind FROM memories WHERE memory_id = ?1";
    let try_conn = |conn: &Connection| -> Option<(String, String, String)> {
        conn.query_row(q, rusqlite::params![memory_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()
        .ok()
        .flatten()
    };
    try_conn(project_conn)
        .or_else(|| user_conn.and_then(try_conn))
        .unwrap_or_else(|| {
            (
                "<unknown — deleted or invalid memory_id>".to_string(),
                String::new(),
                String::new(),
            )
        })
}

fn text_preview(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

fn list_memories_from_conn(conn: &Connection, opts: &ListOptions) -> KimetsuResult<Vec<MemoryRow>> {
    // S4.4 list-asymmetry fix: apply the same active-only filters that
    // `list_user_memories` uses (invalidated_at IS NULL AND superseded_by IS
    // NULL) so that `memory list` on a project brain behaves symmetrically
    // with the user-brain listing — both surfaces show only memories that
    // retrieval would actually return.  Invalidated or superseded memories are
    // still inspectable via the raw DB or the event log.
    let limit = if opts.limit == 0 { 100 } else { opts.limit } as i64;
    let offset = opts.offset as i64;

    let (sql, scope_param): (&str, Option<String>) = if let Some(scope) = opts.scope.as_deref() {
        (
            "
            SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
            FROM memories
            WHERE invalidated_at IS NULL
              AND superseded_by IS NULL
              AND lower(scope) = lower(?1)
            ORDER BY created_at DESC
            LIMIT ?2 OFFSET ?3
            ",
            Some(scope.to_string()),
        )
    } else {
        (
            "
            SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
            FROM memories
            WHERE invalidated_at IS NULL
              AND superseded_by IS NULL
            ORDER BY created_at DESC
            LIMIT ?1 OFFSET ?2
            ",
            None,
        )
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(scope) = scope_param {
        stmt.query_map(params![scope, limit, offset], map_memory_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![limit, offset], map_memory_row)?
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(rows)
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
              AND superseded_by IS NULL
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
              AND superseded_by IS NULL
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
                  AND superseded_by IS NULL
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
                  AND superseded_by IS NULL
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
    sql.push_str(&format!(
        " ORDER BY rowid DESC LIMIT {limit} OFFSET {}",
        filter.offset
    ));

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

    let started = admin_started_event(&paths, &config, run_id, "repo ingest")?;

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

    let finished = admin_finished_event(run_id);
    projector::apply_events(&conn, &[started, ingested, finished])?;

    Ok(summary)
}

/// Ingest files from `files_root` into the brain at `brain_root` (no git
/// discovery; the two roots differ on a server, where the brain lives under the
/// data dir and the files live in a managed checkout). Used by kimetsu-remote's
/// server-side ingest.
pub fn ingest_repo_at_root(
    brain_root: &Path,
    files_root: &Path,
) -> KimetsuResult<RepoIngestSummary> {
    let (mut paths, config, conn) = load_project_at_root(brain_root)?;
    // Walk the checkout, but keep the brain/lock under brain_root.
    paths.repo_root = files_root
        .canonicalize()
        .unwrap_or_else(|_| files_root.to_path_buf());
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain ingest-repo (remote)", Some(run_id))?;

    let started = admin_started_event(&paths, &config, run_id, "repo ingest")?;
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
    let finished = admin_finished_event(run_id);
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

/// v0.6: variant that accepts a full `ContextRequest` so callers can use
/// the new `tags`, `min_score`, `max_capsules`, and `prefer_roles` fields.
pub fn retrieve_context_readonly_with_request(
    start: &Path,
    request: ContextRequest,
) -> KimetsuResult<ContextBundle> {
    BrainSession::open_readonly(start)?.retrieve_context_with_request(request)
}

/// v1.0.0: lexical (FTS-only) read-only retrieval. Used by the
/// `UserPromptSubmit` context-hook so its throwaway per-prompt process
/// never loads the semantic embedding model (a cold ONNX load there can
/// exceed the host's 30s hook timeout). See
/// [`BrainSession::retrieve_context_lexical`].
pub fn retrieve_context_lexical_readonly(
    start: &Path,
    request: ContextRequest,
) -> KimetsuResult<ContextBundle> {
    BrainSession::open_readonly(start)?.retrieve_context_lexical(request)
}

/// v0.8: read-only proactive retrieval (lexical-FTS-only, no model
/// load). The caller builds a `ContextRequest` with `kinds` set to the
/// actionable set, a high `min_score`, and `max_capsules: 1`.
pub fn retrieve_proactive_readonly(
    start: &Path,
    request: ContextRequest,
) -> KimetsuResult<ContextBundle> {
    BrainSession::open_readonly(start)?.retrieve_proactive(request)
}

/// v0.8: full-text search over memory text, for navigating the corpus
/// from the MCP surface. Project rows are paged by `limit`/`offset`;
/// user-brain rows are appended only on the first page so they appear
/// once. Returns empty when the query yields no FTS tokens.
pub fn search_memories(
    start: &Path,
    query: &str,
    limit: u32,
    offset: u32,
    kind: Option<&str>,
    scope: Option<&str>,
) -> KimetsuResult<Vec<MemorySearchHit>> {
    let Some(fts) = context::fts_query(query) else {
        return Ok(Vec::new());
    };
    let (_paths, config, conn) = load_project(start)?;
    let mut hits = search_memories_in_conn(&conn, &fts, limit, offset, kind, scope)?;
    // W3.3: honor config.kimetsu.use_user_brain with env override.
    if offset == 0
        && let Some(user_conn) =
            user_brain::open_user_brain_readonly_for_config(config.kimetsu.use_user_brain)?
    {
        hits.extend(search_memories_in_conn(
            &user_conn, &fts, limit, 0, kind, scope,
        )?);
    }
    Ok(hits)
}

fn search_memories_in_conn(
    conn: &Connection,
    fts_query: &str,
    limit: u32,
    offset: u32,
    kind: Option<&str>,
    scope: Option<&str>,
) -> KimetsuResult<Vec<MemorySearchHit>> {
    let limit = if limit == 0 { 20 } else { limit } as i64;
    let offset = offset as i64;
    let mut sql = String::from(
        "
        SELECT m.memory_id, m.scope, m.kind, m.text, bm25(memories_fts) AS rank
        FROM memories_fts
        JOIN memories m ON m.memory_id = memories_fts.memory_id
        WHERE m.invalidated_at IS NULL
          AND m.superseded_by IS NULL
          AND memories_fts MATCH ?
        ",
    );
    let mut bind: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_query.to_string())];
    if let Some(k) = kind {
        sql.push_str(" AND m.kind = ?");
        bind.push(Box::new(k.to_string()));
    }
    if let Some(s) = scope {
        sql.push_str(" AND lower(m.scope) = lower(?)");
        bind.push(Box::new(s.to_string()));
    }
    // bm25() is more-negative = more-relevant, so ascending rank is best.
    sql.push_str(" ORDER BY rank LIMIT ? OFFSET ?");
    bind.push(Box::new(limit));
    bind.push(Box::new(offset));

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        let raw_rank = row.get::<_, f64>(4)? as f32;
        Ok(MemorySearchHit {
            memory_id: row.get(0)?,
            scope: row.get(1)?,
            kind: row.get(2)?,
            text: row.get(3)?,
            // surface a positive relevance (higher = better) for callers.
            rank: (-raw_rank).max(0.0),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
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
    retrieve_benchmark_context_readonly_with_ambient(
        start,
        task,
        dataset,
        task_slug,
        warm_policy,
        stage,
        budget_tokens,
        require_benchmark_memory,
        max_capsules,
        None,
    )
}

/// v0.4.4: variant that appends an optional ambient-context suffix to
/// the canonical benchmark query AFTER slug detection. Used by the
/// MCP `kimetsu_benchmark_context` tool so the workspace fingerprint
/// (git branch, dirty files, recent edits) contributes to retrieval
/// without corrupting the slug parser.
#[allow(clippy::too_many_arguments)]
pub fn retrieve_benchmark_context_readonly_with_ambient(
    start: &Path,
    task: &str,
    dataset: &str,
    task_slug: Option<&str>,
    warm_policy: benchmark::BenchmarkWarmPolicy,
    stage: &str,
    budget_tokens: u32,
    require_benchmark_memory: bool,
    max_capsules: usize,
    ambient_suffix: Option<&str>,
) -> KimetsuResult<benchmark::BenchmarkBrainContext> {
    let normalized_slug = task_slug
        .and_then(benchmark::normalize_task_slug)
        .or_else(|| benchmark::normalize_task_slug(task));
    let mut query =
        benchmark::benchmark_query(task, dataset, normalized_slug.as_deref(), warm_policy);
    if let Some(suffix) = ambient_suffix.filter(|s| !s.trim().is_empty()) {
        query.push_str(suffix);
    }
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
    let proposal_id = Ulid::new().to_string();
    // v0.4.5: redact secrets in the proposal text + rationale before
    // they hit the memory_proposals table. Benchmark outcomes pull from
    // tool output, which is exactly where a model-leaked token would surface.
    let raw_text = benchmark::proposal_memory_text(outcome, proposal);
    let text_redaction = redact::redact_secrets(&raw_text);
    if text_redaction.was_redacted() {
        eprintln!(
            "kimetsu-brain (benchmark proposal): {}",
            text_redaction.summary()
        );
    }
    let text = text_redaction.text;
    let kind = benchmark::proposal_memory_kind(proposal);
    let rationale_raw = if proposal.rationale.trim().is_empty() {
        "generalized from benchmark outcome".to_string()
    } else {
        proposal.rationale.trim().to_string()
    };
    let rationale = redact::redact_secrets(&rationale_raw).text;

    let started = admin_started_event(&paths, &config, run_id, "benchmark memory proposal")?;

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

    let finished = admin_finished_event(run_id);

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

    let finished = admin_finished_event(run_id);

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

    let invalidated = Event::new(
        run_id,
        "memory.invalidated",
        serde_json::json!({
            "memory_id": memory_id,
            "reason": resolved_reason,
        }),
    );

    let finished = admin_finished_event(run_id);

    projector::apply_events(&conn, &[started, invalidated, finished])?;
    Ok(())
}

/// QoL: returned by [`undo_last_memory`] — the memory that was just invalidated.
#[derive(Debug, Clone)]
pub struct UndoneMemory {
    pub memory_id: String,
    pub text: String,
    pub scope: String,
    pub kind: String,
}

/// QoL: edit an existing active memory in-place, preserving its usefulness history.
///
/// - `new_text`: if given, the text (and normalized_text) are updated, the FTS
///   index row is refreshed, and a new embedding is stored via the configured
///   embedder (no-op in lean builds). Secret-redaction is applied at the same
///   boundary as `add_memory`.
/// - `new_kind`: if given, the `kind` column is updated.
///
/// At least one of `new_text` / `new_kind` must be `Some`; otherwise an error
/// is returned. `use_count`, `usefulness_score`, `confidence`, and `created_at`
/// are intentionally left unchanged — the whole point of edit-in-place is to
/// preserve the memory's learned history.
///
/// Errors if the memory id is unknown or already invalidated.
pub fn edit_memory(
    start: &Path,
    memory_id: &str,
    new_text: Option<&str>,
    new_kind: Option<MemoryKind>,
) -> KimetsuResult<()> {
    if new_text.is_none() && new_kind.is_none() {
        return Err("edit_memory: at least one of --text or --kind must be provided".into());
    }

    let (paths, config, conn) = load_project(start)?;

    // Verify memory exists and is active (not invalidated).
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT scope, kind, invalidated_at FROM memories WHERE memory_id = ?1",
            params![memory_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2).unwrap_or_default(),
                ))
            },
        )
        .optional()?;

    let (scope, current_kind, invalidated_at) = match row {
        None => return Err(format!("memory not found: {memory_id}").into()),
        Some(r) => r,
    };
    if !invalidated_at.is_empty() {
        return Err(format!("memory {memory_id} is already invalidated").into());
    }

    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory edit", Some(run_id))?;

    // Apply text update.
    if let Some(raw_text) = new_text {
        let redaction = redact::redact_secrets(raw_text);
        if redaction.was_redacted() {
            eprintln!("kimetsu-brain: {}", redaction.summary());
        }
        let text = &redaction.text;
        let normalized = normalize_memory_text(text);

        conn.execute(
            "UPDATE memories SET text = ?1, normalized_text = ?2 WHERE memory_id = ?3",
            params![text, normalized, memory_id],
        )?;

        // Refresh the FTS index row.
        conn.execute(
            "DELETE FROM memories_fts WHERE memory_id = ?1",
            params![memory_id],
        )?;
        let kind_for_fts = new_kind
            .as_ref()
            .map(|k| k.to_string())
            .unwrap_or(current_kind.clone());
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope) VALUES (?1, ?2, ?3, ?4)",
            params![memory_id, text, kind_for_fts, scope],
        )?;

        // Re-embed so semantic retrieval reflects the corrected text.
        let embedder = embeddings::open_embedder_for(config.embedder.enabled);
        embeddings::embed_and_persist(&conn, memory_id, text, embedder)?;
        // (return value not needed here — no conflict scan after an edit)
    }

    // Apply kind update (FTS row may need refreshing if text wasn't also changed).
    if let Some(kind) = new_kind {
        conn.execute(
            "UPDATE memories SET kind = ?1 WHERE memory_id = ?2",
            params![kind.to_string(), memory_id],
        )?;

        // Only refresh FTS kind column if we didn't already rebuild it above.
        if new_text.is_none() {
            // Re-read the current text from DB to rebuild the FTS row with
            // the new kind (text unchanged).
            let current_text: String = conn.query_row(
                "SELECT text FROM memories WHERE memory_id = ?1",
                params![memory_id],
                |row| row.get(0),
            )?;
            conn.execute(
                "DELETE FROM memories_fts WHERE memory_id = ?1",
                params![memory_id],
            )?;
            conn.execute(
                "INSERT INTO memories_fts (memory_id, text, kind, scope) VALUES (?1, ?2, ?3, ?4)",
                params![memory_id, current_text, kind.to_string(), scope],
            )?;
        }
    }

    Ok(())
}

/// QoL: return the most recently created active memory in the project brain
/// WITHOUT invalidating it — used by the CLI to show a preview before
/// asking for confirmation. Returns `Ok(None)` if there are no active memories.
pub fn peek_last_memory(start: &Path) -> KimetsuResult<Option<UndoneMemory>> {
    let (_paths, _config, conn) = load_project(start)?;
    // S4.4b: exclude superseded rows — a retired/merged memory is not a
    // sensible "last" memory to surface to the user.
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT memory_id, text, scope, kind FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
             ORDER BY created_at DESC, memory_id DESC
             LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    Ok(row.map(|(memory_id, text, scope, kind)| UndoneMemory {
        memory_id,
        text,
        scope,
        kind,
    }))
}

/// QoL: invalidate the most recently created active memory in the project brain.
///
/// Finds the newest ACTIVE (non-invalidated) memory, invalidates it with the
/// reason `"undo: last recorded memory"`, and returns its details. Returns
/// `Ok(None)` when there are no active memories in the project brain.
///
/// Operates on the PROJECT brain only (the "agent just saved junk in this
/// project" case); the user brain is not touched.
pub fn undo_last_memory(start: &Path) -> KimetsuResult<Option<UndoneMemory>> {
    let (paths, _config, conn) = load_project(start)?;

    // S4.4b: exclude superseded rows — undoing a retired/merged memory would
    // confuse the user; they should undo the survivor instead.
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT memory_id, text, scope, kind FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
             ORDER BY created_at DESC, memory_id DESC
             LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    let (memory_id, text, scope, kind) = match row {
        None => return Ok(None),
        Some(r) => r,
    };

    // Release the read conn before calling invalidate_memory which opens its own.
    drop(conn);
    drop(paths);

    invalidate_memory(start, &memory_id, Some("undo: last recorded memory"))?;

    Ok(Some(UndoneMemory {
        memory_id,
        text,
        scope,
        kind,
    }))
}

pub fn reject_proposal(start: &Path, proposal_id: &str, reason: Option<&str>) -> KimetsuResult<()> {
    let (paths, config, conn) = load_project(start)?;
    let _proposal = load_pending_proposal(&conn, proposal_id)?;
    let run_id = RunId::new();
    let _lock = ProjectLock::acquire(&paths, "brain memory reject", Some(run_id))?;

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

    let rejected = Event::new(
        run_id,
        "memory.rejected",
        serde_json::json!({
            "proposal_id": proposal_id,
            "reason": resolved_reason,
        }),
    );

    let finished = admin_finished_event(run_id);

    projector::apply_events(&conn, &[started, rejected, finished])?;
    Ok(())
}

pub fn rebuild_projection(start: &Path, from_traces: bool) -> KimetsuResult<usize> {
    let (paths, _config, conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "brain rebuild", None)?;

    // Explicit legacy import: rebuild from on-disk trace.jsonl files (inserts
    // any events missing from the table via OR IGNORE, then projects).
    if from_traces {
        let events = trace::read_all_traces(&paths)?;
        projector::rebuild(&conn, &events)?;
        return Ok(events.len());
    }

    // Auto-fallback: a brain whose events table was wiped by a pre-W1.1 rebuild
    // still has its history only in trace.jsonl. If the table is empty but
    // traces exist, import them first, then proceed.
    let event_count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
    if event_count == 0 {
        let events = trace::read_all_traces(&paths)?;
        if !events.is_empty() {
            eprintln!(
                "[kimetsu] events table empty; importing {} event(s) from legacy traces",
                events.len()
            );
            projector::rebuild(&conn, &events)?;
            return Ok(events.len());
        }
    }

    // Normal path: replay the durable events table in place.
    projector::rebuild_in_place(&conn)
}

pub fn clear_lock(start: &Path) -> KimetsuResult<bool> {
    let paths = ProjectPaths::discover(start)?;
    crate::lock::clear_force(&paths)
}

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

/// D2: Abort a dangling run — cleanly finalize a run that has no terminal
/// event (e.g. the process was killed mid-way). Steps:
///
/// 1. Validate the run_id exists in `runs`.
/// 2. Error if the run already has a `terminal_kind` (already finished/failed/aborted).
/// 3. Append a `run.aborted` event to the run's trace.
/// 4. Project it (updates `runs.ended_at` + `terminal_kind`).
/// 5. Clear any stale writer lock so subsequent commands can proceed.
///
/// Returns the trace path on success. Errors if the run is unknown or already terminal.
pub fn abort_run(start: &Path, run_id_str: &str) -> KimetsuResult<()> {
    // 1. Validate the run_id exists + check terminal state (read-only query).
    {
        let (_paths, _config, ro_conn) = load_project_readonly(start)?;
        let row: Option<Option<String>> = ro_conn
            .query_row(
                "SELECT terminal_kind FROM runs WHERE run_id = ?1",
                rusqlite::params![run_id_str],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        match row {
            None => {
                return Err(format!("run abort: unknown run_id `{run_id_str}`").into());
            }
            Some(Some(terminal_kind)) => {
                return Err(format!(
                    "run abort: run `{run_id_str}` is already terminal ({})",
                    terminal_kind
                )
                .into());
            }
            Some(None) => {} // dangling — proceed
        }
    }

    // 2. Parse the run_id as a RunId.
    let run_id: RunId = run_id_str
        .parse::<ulid::Ulid>()
        .map(RunId)
        .map_err(|_| format!("run abort: `{run_id_str}` is not a valid ULID run id"))?;

    // 3. Open rw, append run.aborted, project it.
    let (paths, _config, conn) = load_project(start)?;
    let lock = ProjectLock::acquire(&paths, "run abort", Some(run_id))?;

    // Open the trace in append mode (create_dirs is idempotent).
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;

    let aborted_event = Event::new(
        run_id,
        "run.aborted",
        serde_json::json!({
            "reason": "manual_abort_via_cli",
        }),
    );
    writer.append(&aborted_event, true)?;
    projector::apply_events(&conn, &[aborted_event])?;

    // 4. Release the write lock acquired above, then force-clear any
    //    additional stale lock file that may have been left by a
    //    previously killed process (clear_force is idempotent).
    lock.release()?;
    crate::lock::clear_force(&paths)?;

    Ok(())
}

/// C7: best-effort telemetry write from a hook context (no active run).
///
/// Appends a single event (e.g. `context.served`) directly to the project
/// brain's `events` table with a sentinel run_id (`"hook"` encoded as a
/// ULID-zero string). Swallows all errors — telemetry must never break
/// a hook. Opens the DB read-write so the hook can record misses without
/// holding a write lock (the DB is opened and closed immediately).
///
/// The sentinel run_id is a valid ULID-shaped string (`00000000000000000000000000`
/// padded to 26 chars). Crucially there is **no** corresponding row in the
/// `runs` table; analytics windows over `context.served` filter by `ts`, not
/// `run_id`, so this is correct.
pub fn log_telemetry_event(
    start: &Path,
    kind: &str,
    payload: serde_json::Value,
) -> KimetsuResult<()> {
    // We need a read-write connection to insert. Use a fresh Connection
    // (not load_project which also validates config) so a misconfigured
    // project.toml never prevents telemetry from writing.
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    // Sentinel run_id: all-zero ULID (26 '0' chars), never in `runs`.
    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let event = Event::new(sentinel_run_id, kind, payload);
    projector::insert_event(&conn, &event)?;
    Ok(())
}

/// v1.5: scan `events` for `memory.cited` entries and, for each cited
/// memory id, check the dropped-capsule sidecar. When a cited memory
/// was in the recent-dropped window (it was excluded by the relevance
/// floor but the model cited it anyway), emit a `retrieval.regret`
/// telemetry event and remove the entry from the sidecar.
///
/// Purely best-effort: any sidecar or telemetry error is swallowed so
/// citation recording is never disrupted. Called from the pipeline
/// after `projector::apply_events` so citations are already in the DB
/// before we check for regrets.
///
/// Cross-process note: the sidecar is written by the `brain_context_hook`
/// (CLI process) and read here by the pipeline / MCP-server process.
/// Both derive the same cache dir from the repo root, so they
/// naturally share the file without coordination.
pub fn emit_regret_for_cited_memories(start: &Path, events: &[kimetsu_core::event::Event]) {
    use crate::dropped_capsule;
    use kimetsu_core::paths::{ProjectPaths, user_cache_dir_for};

    // Derive the project cache dir; silently skip if the brain is not
    // initialised (e.g. during one-off tests that don't init a project).
    let cache_dir = match ProjectPaths::discover(start) {
        Ok(paths) => user_cache_dir_for(&paths.repo_root),
        Err(_) => return,
    };

    let cited_at = dropped_capsule::now_secs();

    for event in events {
        if event.kind != "memory.cited" {
            continue;
        }
        let Some(memory_id) = event.payload.get("memory_id").and_then(|v| v.as_str()) else {
            continue;
        };
        // Best-effort: swallow any sidecar error.
        let Some(dropped_entry) = dropped_capsule::take_if_dropped(&cache_dir, memory_id, cited_at)
        else {
            continue;
        };
        // Emit the regret event.
        let _ = log_telemetry_event(
            start,
            "retrieval.regret",
            serde_json::json!({
                "memory_id": memory_id,
                "dropped_at": dropped_entry.dropped_at,
                "cited_at": cited_at,
            }),
        );
    }
}

/// v1.5: write a `memory.cited` event from the MCP `kimetsu_brain_cite` tool.
///
/// Uses the same sentinel run_id as [`log_telemetry_event`] (all-zero ULID)
/// so no corresponding `runs` row is required. The event is inserted then
/// projected (populating `memory_citations`) in one connection, and the
/// regret sidecar is checked best-effort.
pub fn record_mcp_citation(start: &Path, memory_id: &str, note: Option<&str>) -> KimetsuResult<()> {
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let mut payload = serde_json::json!({
        "memory_id": memory_id,
        "turn": 0,
    });
    if let Some(n) = note {
        payload["rationale"] = serde_json::json!(n);
    }
    let event = kimetsu_core::event::Event::new(sentinel_run_id, "memory.cited", payload);
    // apply_events calls insert_event + project_event in one transaction.
    projector::apply_events(&conn, std::slice::from_ref(&event))?;

    // Best-effort regret check.
    emit_regret_for_cited_memories(start, std::slice::from_ref(&event));

    Ok(())
}

/// Inject a `retrieval.regret` telemetry event for a memory.
///
/// The auto-path ([`emit_regret_for_cited_memories`]) only fires when a memory
/// was dropped by a retrieval floor and then cited anyway. This is the explicit
/// path (the `kimetsu brain regret` CLI / benchmarks): it records the negative
/// signal directly so lifecycle review and calibration can be exercised without
/// reproducing the full drop-then-cite dance.
pub fn record_regret(start: &Path, memory_id: &str) -> KimetsuResult<()> {
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    // Use the sentinel run id (no active run) and PROJECT the event so the
    // outcome handler (`apply_retrieval_regret`) runs live, not just on rebuild.
    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let event = kimetsu_core::event::Event::new(
        sentinel_run_id,
        "retrieval.regret",
        serde_json::json!({ "memory_id": memory_id, "source": "manual" }),
    );
    projector::apply_events(&conn, std::slice::from_ref(&event))?;
    Ok(())
}

/// Backdate a memory's `created_at` / `last_useful_at` by `days_ago` days via a
/// `memory.aged` event. A testing/benchmark affordance for exercising
/// age-sensitive policies (forgetting). The absolute target timestamp is stored
/// in the event payload, so replay on rebuild is deterministic.
pub fn record_set_age(start: &Path, memory_id: &str, days_ago: u32) -> KimetsuResult<()> {
    use time::format_description::well_known::Rfc3339;

    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let target = time::OffsetDateTime::now_utc() - time::Duration::days(days_ago as i64);
    let ts = target.format(&Rfc3339).unwrap_or_default();

    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let event = kimetsu_core::event::Event::new(
        sentinel_run_id,
        "memory.aged",
        serde_json::json!({ "memory_id": memory_id, "created_at": ts, "last_useful_at": ts }),
    );
    projector::apply_events(&conn, std::slice::from_ref(&event))?;
    Ok(())
}

// ── #2 knowledge graph: build relation edges ─────────────────────────────────

/// Summary of a `kimetsu brain graph build` run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphBuildSummary {
    /// Active (non-invalidated, non-superseded) memories scanned.
    pub active_memories: usize,
    /// Rule-derived `relates_to` edges proposed.
    pub rule_edges: usize,
    /// Enrichment (LLM typed) edges proposed by the caller.
    pub enrich_edges: usize,
    /// Edges actually written (0 when `dry_run`).
    pub written: usize,
    /// Proposed edge counts grouped by edge_type (rule + enrichment, pre-write).
    pub by_type: std::collections::BTreeMap<String, usize>,
    /// True when no edges were persisted (preview only).
    pub dry_run: bool,
}

/// Read every active memory as `(id, text)` for graph enrichment. Read-only.
/// Exposed so the CLI (which owns the cheap-model provider) can compute typed
/// enrichment edges before calling [`build_graph`].
pub fn active_memory_texts(start: &Path) -> KimetsuResult<Vec<(String, String)>> {
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open_with_flags(&paths.brain_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT memory_id, text
         FROM memories
         WHERE invalidated_at IS NULL AND superseded_by IS NULL
         ORDER BY memory_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// #2: build the knowledge-graph edges for the workspace brain.
///
/// Combines the deterministic rule layer ([`crate::graph::build_relates_to_edges`])
/// with any caller-supplied `extra_edges` (LLM enrichment computed in the CLI),
/// de-duplicates, and — unless `dry_run` — persists them as rebuild-safe
/// `memory.edge` events via [`projector::add_memory_edges`]. Returns a summary.
///
/// `max_fan_out` caps rule edges per source memory (0 = the module default).
pub fn build_graph(
    start: &Path,
    extra_edges: &[(String, String, String)],
    max_fan_out: usize,
    dry_run: bool,
) -> KimetsuResult<GraphBuildSummary> {
    use std::collections::{BTreeMap, BTreeSet};

    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let active_memories = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL AND superseded_by IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;

    let rule = crate::graph::build_relates_to_edges(&conn, max_fan_out)?;
    let rule_edges = rule.len();
    let enrich_edges = extra_edges.len();

    // Merge rule + enrichment, de-duplicating on (src, dst, type). Self-loops are
    // dropped by add_memory_edges; we also drop them here for an accurate summary.
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut merged: Vec<(String, String, String)> = Vec::new();
    let push = |src: String,
                dst: String,
                ty: String,
                seen: &mut BTreeSet<(String, String, String)>,
                by_type: &mut BTreeMap<String, usize>,
                merged: &mut Vec<(String, String, String)>| {
        if src == dst {
            return;
        }
        let key = (src.clone(), dst.clone(), ty.clone());
        if seen.insert(key) {
            *by_type.entry(ty.clone()).or_insert(0) += 1;
            merged.push((src, dst, ty));
        }
    };
    for e in &rule {
        push(
            e.src_id.clone(),
            e.dst_id.clone(),
            e.edge_type.clone(),
            &mut seen,
            &mut by_type,
            &mut merged,
        );
    }
    for (src, dst, ty) in extra_edges {
        push(
            src.clone(),
            dst.clone(),
            ty.clone(),
            &mut seen,
            &mut by_type,
            &mut merged,
        );
    }

    let written = if dry_run {
        0
    } else {
        projector::add_memory_edges(&conn, &merged)?
    };

    Ok(GraphBuildSummary {
        active_memories,
        rule_edges,
        enrich_edges,
        written,
        by_type,
        dry_run,
    })
}

// ── Q5: portable memory export / import ──────────────────────────────────────

/// A single memory in the portable JSON exchange format.
///
/// Carries only the fields needed to reconstruct the memory in another brain —
/// instance-specific data (`memory_id`, `usefulness_score`, `use_count`) is
/// intentionally excluded so importing always creates a fresh row with clean
/// stats.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryExport {
    pub text: String,
    pub scope: String,
    pub kind: String,
    pub confidence: f32,
    pub created_at: Option<String>,
}

/// v3.0 #4: a shareable brain PACK — a self-describing envelope (manifest +
/// memories) for distribution via the marketplace. Serialized to JSON then
/// gzip-compressed by the CLI. A bare `Vec<MemoryExport>` (the pre-pack export
/// format) also imports, for back-compat — see [`parse_pack_or_array`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Pack {
    /// Pack format version (currently 1).
    pub kimetsu_pack: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<String>,
    #[serde(default)]
    pub memory_count: usize,
    pub memories: Vec<MemoryExport>,
}

/// Identity of an installed pack, stamped into each imported memory's provenance
/// so it can later be listed / updated / uninstalled.
#[derive(Debug, Clone, Default)]
pub struct PackRef {
    pub name: Option<String>,
    pub version: Option<String>,
}

/// Parse a pack file body: a [`Pack`] envelope OR a bare `Vec<MemoryExport>`
/// (back-compat with pre-pack exports). Returns the manifest [`PackRef`] (empty
/// for a bare array) and the memory entries.
pub fn parse_pack_or_array(json: &str) -> KimetsuResult<(PackRef, Vec<MemoryExport>)> {
    // A Pack is a JSON object with `kimetsu_pack` + `memories`; a bare array is
    // a JSON array. Try the envelope first; fall back to the array.
    if let Ok(pack) = serde_json::from_str::<Pack>(json) {
        return Ok((
            PackRef {
                name: pack.name,
                version: pack.version,
            },
            pack.memories,
        ));
    }
    let entries: Vec<MemoryExport> = serde_json::from_str(json)
        .map_err(|e| format!("pack: not a Pack envelope or a memory array: {e}"))?;
    Ok((PackRef::default(), entries))
}

/// Strip the trailing `(context: …)` segment from a memory text produced by
/// the distiller / `brain record` workflow, leaving only the lesson body.
///
/// Matches the literal pattern ` (context: <anything>)` at the very end of
/// the trimmed string. The match is case-sensitive to avoid false positives.
///
/// Returns the original `text` unchanged when:
///   - the pattern is absent, or
///   - stripping would leave an empty or whitespace-only string (safety
///     fallback: a blank lesson is worse than a slightly noisy one).
///
/// # Examples
/// ```
/// # use kimetsu_brain::project::redact_context_suffix;
/// assert_eq!(
///     redact_context_suffix("always use --locked (context: cargo build)"),
///     "always use --locked"
/// );
/// assert_eq!(
///     redact_context_suffix("bare lesson"),
///     "bare lesson"
/// );
/// ```
pub fn redact_context_suffix(text: &str) -> &str {
    let trimmed = text.trim_end();
    // Pattern: " (context: …)" where the parenthesised segment is at the end.
    // Walk backwards to find the matching open-paren for a ` (context: ` prefix.
    if let Some(pos) = find_trailing_context_paren(trimmed) {
        let candidate = trimmed[..pos].trim_end();
        if !candidate.is_empty() {
            return candidate;
        }
    }
    text
}

/// Strip the leading `[tags: …]` prefix from a memory text, leaving only the
/// lesson body (and any trailing context segment unless that is separately
/// stripped by [`redact_context_suffix`]).
///
/// Matches `[tags: …] ` at the very start of the trimmed string.
/// Returns the original `text` when:
///   - the pattern is absent, or
///   - stripping would leave an empty or whitespace-only string.
///
/// # Examples
/// ```
/// # use kimetsu_brain::project::redact_tags_prefix;
/// assert_eq!(
///     redact_tags_prefix("[tags: rust, cargo] always use --locked"),
///     "always use --locked"
/// );
/// assert_eq!(
///     redact_tags_prefix("no tags here"),
///     "no tags here"
/// );
/// ```
pub fn redact_tags_prefix(text: &str) -> &str {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("[tags: ") {
        if let Some(close) = rest.find(']') {
            let after = rest[close + 1..].trim_start();
            if !after.is_empty() {
                return after;
            }
        }
    }
    text
}

/// Apply export-time redaction to a single `MemoryExport`'s text field
/// according to the requested flags. Returns a new `MemoryExport` with the
/// text replaced (or the original when no patterns match and the safety
/// fallback applies).
///
/// The two-step order matters: strip tags first, then context, so that a
/// memory like `[tags: rust] lesson body (context: foo)` becomes
/// `lesson body` when both flags are active.
pub fn apply_export_redaction(
    entry: MemoryExport,
    redact: bool,
    redact_tags: bool,
) -> MemoryExport {
    if !redact && !redact_tags {
        return entry;
    }
    let mut text: &str = &entry.text;
    // Temporary storage so we can chain borrows without lifetime woes.
    let after_tags: String;
    let after_ctx: String;
    if redact_tags {
        let stripped = redact_tags_prefix(text);
        after_tags = stripped.to_string();
        text = &after_tags;
    }
    if redact {
        let stripped = redact_context_suffix(text);
        after_ctx = stripped.to_string();
        text = &after_ctx;
    }
    MemoryExport {
        text: text.to_string(),
        ..entry
    }
}

// Helper: find the byte offset of the opening ` (context: ` run that closes
// at the very end of `s` (which must already be trimmed of trailing
// whitespace). Returns `None` when no such suffix is present.
fn find_trailing_context_paren(s: &str) -> Option<usize> {
    // We look for a closing `)` at the end, then walk left to find ` (context: `.
    if !s.ends_with(')') {
        return None;
    }
    // The minimum suffix is ` (context: x)` — 13 chars.
    let bytes = s.as_bytes();
    // Find the matching open paren by scanning backwards from the terminal `)`.
    let close = s.len() - 1;
    // We need at least " (context: " before the close paren, so start scanning
    // no further than close - len(" (context: ") = close - 11.
    // Use a simple prefix search scanning from the right.
    let prefix = b" (context: ";
    for start in (0..close).rev() {
        if start + prefix.len() > close {
            continue;
        }
        if &bytes[start..start + prefix.len()] == prefix {
            // Found the open sequence; the segment is s[start..=close].
            return Some(start);
        }
    }
    None
}

/// Summary returned by [`import_memories`] / [`import_pack`].
#[derive(Debug, Clone, Default)]
pub struct ImportSummary {
    /// Memories that were actually written (new rows).
    pub imported: usize,
    /// Entries that were skipped because an identical memory already existed
    /// (detected by `add_memory`'s normalized-text dedup) or because the
    /// scope/kind was malformed.
    pub deduped: usize,
    /// v3.0 #4: memories superseded by a `replace`-mode pack install (existing
    /// active memories in the pack's scope(s), invalidated before the load).
    pub superseded: usize,
}

thread_local! {
    /// v3.0 #4: provenance source stamped onto memories written during a pack
    /// install (e.g. `{source:"pack", pack_name, pack_version}`). When unset,
    /// `add_memory` uses its default `manual_cli` provenance. RAII-scoped by
    /// [`ImportProvenanceScope`] so it never leaks past the import.
    static IMPORT_PROVENANCE: std::cell::RefCell<Option<serde_json::Value>> =
        const { std::cell::RefCell::new(None) };
}

struct ImportProvenanceScope;
impl ImportProvenanceScope {
    fn new(v: serde_json::Value) -> Self {
        IMPORT_PROVENANCE.with(|c| *c.borrow_mut() = Some(v));
        ImportProvenanceScope
    }
}
impl Drop for ImportProvenanceScope {
    fn drop(&mut self) {
        IMPORT_PROVENANCE.with(|c| *c.borrow_mut() = None);
    }
}

/// Build a memory's `provenance_snapshot`. Uses the thread-local pack source
/// (set during a pack install) when present, else the default `manual_cli`.
fn build_provenance(run_id: RunId, text: &str) -> serde_json::Value {
    IMPORT_PROVENANCE.with(|c| {
        if let Some(src) = c.borrow().as_ref() {
            let mut v = src.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("run_id".into(), serde_json::json!(run_id.to_string()));
                obj.insert("text".into(), serde_json::json!(text));
            }
            v
        } else {
            serde_json::json!({
                "source": "manual_cli",
                "run_id": run_id.to_string(),
                "text": text,
            })
        }
    })
}

/// Export active memories as a vec of portable records.
///
/// `scope` and `kind` are optional filters; `None` means "all".
/// `redact` strips the trailing `(context: …)` segment from each text.
/// `redact_tags` additionally strips the leading `[tags: …]` prefix.
/// Aggregate security-scrub findings across an export (no credentials / PII may
/// ship in a shareable pack). `kinds` maps each redaction kind to its count.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ScrubReport {
    pub total: usize,
    pub kinds: std::collections::BTreeMap<String, usize>,
}

impl ScrubReport {
    pub fn is_clean(&self) -> bool {
        self.total == 0
    }
    /// One-liner like `"scrubbed 4: email×2, anthropic_oauth×1, ssn×1"`.
    pub fn summary(&self) -> String {
        if self.total == 0 {
            return "no credentials or PII found".to_string();
        }
        let parts: Vec<String> = self.kinds.iter().map(|(k, n)| format!("{k}×{n}")).collect();
        format!("scrubbed {}: {}", self.total, parts.join(", "))
    }
}

pub fn export_memories(
    start: &Path,
    scope: Option<MemoryScope>,
    kind: Option<MemoryKind>,
    redact: bool,
    redact_tags: bool,
) -> KimetsuResult<(Vec<MemoryExport>, ScrubReport)> {
    // Build the SQL dynamically based on the optional filters, including
    // `created_at` so the JSON record carries the origin timestamp.
    let (sql, params_vec): (&str, Vec<String>) = match (scope.as_ref(), kind.as_ref()) {
        (Some(s), Some(k)) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND lower(scope) = lower(?1)
               AND lower(kind)  = lower(?2)
             ORDER BY created_at DESC",
            vec![s.to_string(), k.to_string()],
        ),
        (Some(s), None) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND lower(scope) = lower(?1)
             ORDER BY created_at DESC",
            vec![s.to_string()],
        ),
        (None, Some(k)) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND lower(kind) = lower(?1)
             ORDER BY created_at DESC",
            vec![k.to_string()],
        ),
        (None, None) => (
            "SELECT scope, kind, text, confidence, created_at
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
             ORDER BY created_at DESC",
            vec![],
        ),
    };

    // Project-level memories only (user brain memories live in a separate DB;
    // callers wanting the user brain should call with scope=GlobalUser on the
    // user-brain path, or simply use list_memories which merges both).
    let (_paths, _config, conn) = load_project(start)?;

    let mut stmt = conn.prepare(sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params_vec
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(MemoryExport {
            scope: row.get(0)?,
            kind: row.get(1)?,
            text: row.get(2)?,
            confidence: row.get::<_, f64>(3)? as f32,
            created_at: row.get(4)?,
        })
    })?;

    // Security scrub (v3.0 #4): every exported memory passes through the
    // credential + PII scrubber so a shareable pack can never ship secrets or
    // personal data. The scrub is on the EXPORT COPY only — the source DB is
    // untouched. Findings are tallied for the caller to report (and --strict).
    let mut out = Vec::new();
    let mut report = ScrubReport::default();
    for row in rows {
        let mut entry = apply_export_redaction(row?, redact, redact_tags);
        let scrubbed = crate::redact::scrub_for_export(&entry.text);
        for m in &scrubbed.matches {
            *report.kinds.entry(m.kind.to_string()).or_insert(0) += 1;
            report.total += 1;
        }
        entry.text = scrubbed.text;
        out.push(entry);
    }
    Ok((out, report))
}

/// Import a slice of [`MemoryExport`] records into the brain at `start`.
///
/// For each entry:
/// - Parse scope + kind from the string fields (with optional `scope_override`).
/// - Call `add_memory`, which dedups by normalized text. Dedup is detected by
///   comparing the set of active memory IDs in the project DB before vs after
///   each `add_memory` call — if the returned ID was already in the DB at
///   the start of this import batch, it counts as deduped.
/// - Malformed entries (bad scope/kind string) are skipped with a warning;
///   they do NOT abort the whole import.
///
/// Returns an [`ImportSummary`] with `imported` (new rows) and `deduped`
/// (entries that collapsed to an existing row or were skipped).
pub fn import_memories(
    start: &Path,
    entries: &[MemoryExport],
    scope_override: Option<MemoryScope>,
) -> KimetsuResult<ImportSummary> {
    let mut summary = ImportSummary::default();

    // Snapshot all active memory IDs before we start importing.  Any ID
    // returned by add_memory that is already in this set is a dedup.
    let pre_existing_ids: std::collections::HashSet<String> = {
        // Open a read-only connection just for the snapshot; avoid holding it
        // across the write calls (each add_memory opens its own connection).
        match load_project_readonly(start) {
            Ok((_paths, _config, conn)) => {
                let mut stmt = conn
                    .prepare("SELECT memory_id FROM memories WHERE invalidated_at IS NULL")
                    .unwrap_or_else(|_| conn.prepare("SELECT memory_id FROM memories").unwrap());
                stmt.query_map([], |row| row.get::<_, String>(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            }
            Err(_) => std::collections::HashSet::new(),
        }
    };

    // Also track IDs minted during THIS batch so we can detect within-batch
    // duplicates (e.g. two identical entries in the import file).
    let mut this_batch_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in entries {
        // Resolve scope: prefer override, then parse from the entry.
        let scope = if let Some(ref ov) = scope_override {
            *ov
        } else {
            match entry.scope.parse::<MemoryScope>() {
                Ok(s) => s,
                Err(_) => {
                    eprintln!(
                        "kimetsu-brain import: skipping entry with unknown scope `{}`",
                        entry.scope
                    );
                    summary.deduped += 1;
                    continue;
                }
            }
        };

        // Resolve kind.
        let kind = match entry.kind.parse::<MemoryKind>() {
            Ok(k) => k,
            Err(_) => {
                eprintln!(
                    "kimetsu-brain import: skipping entry with unknown kind `{}`",
                    entry.kind
                );
                summary.deduped += 1;
                continue;
            }
        };

        match add_memory(start, scope, kind, &entry.text) {
            Ok(id) => {
                // Dedup if the ID was present before this import started OR
                // was already seen in this batch (within-batch duplicates).
                if pre_existing_ids.contains(&id) || !this_batch_ids.insert(id) {
                    summary.deduped += 1;
                } else {
                    summary.imported += 1;
                }
            }
            Err(e) => {
                eprintln!("kimetsu-brain import: failed to add memory: {e}");
                summary.deduped += 1;
            }
        }
    }

    Ok(summary)
}

/// v3.0 #4: install a pack's memories. `merge` adds additively (dedup against
/// existing). `replace` first invalidates active memories in the pack's scope(s)
/// — REVERSIBLE (events kept; rows marked invalidated) — then loads the pack.
/// Each installed memory is stamped with the `pack` provenance.
pub fn import_pack(
    start: &Path,
    entries: &[MemoryExport],
    scope_override: Option<MemoryScope>,
    replace: bool,
    pack: Option<&PackRef>,
) -> KimetsuResult<ImportSummary> {
    let mut superseded = 0usize;
    if replace {
        let scopes = pack_target_scopes(entries, scope_override);
        let reason = match pack {
            Some(p) => format!(
                "replaced_by_pack:{}@{}",
                p.name.as_deref().unwrap_or("unknown"),
                p.version.as_deref().unwrap_or("?")
            ),
            None => "replaced_by_import".to_string(),
        };
        for id in active_memory_ids_in_scopes(start, &scopes)? {
            invalidate_memory(start, &id, Some(&reason))?;
            superseded += 1;
        }
    }

    // Defensive scrub: never INGEST a credential/PII from a pack, even if the
    // author bypassed export-time scrubbing. (Export already scrubs; this is
    // belt-and-suspenders on the receiving side.)
    let scrubbed: Vec<MemoryExport> = entries
        .iter()
        .map(|e| {
            let mut e = e.clone();
            e.text = crate::redact::scrub_for_export(&e.text).text;
            e
        })
        .collect();

    // Stamp pack provenance on each installed memory for the duration of the load.
    let _prov = pack.map(|p| {
        ImportProvenanceScope::new(serde_json::json!({
            "source": "pack",
            "pack_name": p.name,
            "pack_version": p.version,
        }))
    });
    let mut summary = import_memories(start, &scrubbed, scope_override)?;
    summary.superseded = superseded;
    Ok(summary)
}

/// Distinct scopes a pack will write to (override wins; else parsed per entry).
fn pack_target_scopes(
    entries: &[MemoryExport],
    scope_override: Option<MemoryScope>,
) -> Vec<MemoryScope> {
    if let Some(ov) = scope_override {
        return vec![ov];
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in entries {
        if let Ok(s) = e.scope.parse::<MemoryScope>() {
            if seen.insert(s.to_string()) {
                out.push(s);
            }
        }
    }
    out
}

/// Active (non-invalidated, non-superseded) memory ids in the given scopes.
fn active_memory_ids_in_scopes(start: &Path, scopes: &[MemoryScope]) -> KimetsuResult<Vec<String>> {
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let (_p, _c, conn) = load_project_readonly(start)?;
    let mut ids = Vec::new();
    for sc in scopes {
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM memories
             WHERE scope = ?1 AND invalidated_at IS NULL AND superseded_by IS NULL",
        )?;
        let rows = stmt.query_map(params![sc.to_string()], |r| r.get::<_, String>(0))?;
        for r in rows {
            ids.push(r?);
        }
    }
    Ok(ids)
}

// ── Q8: brain compact ────────────────────────────────────────────────────────

/// Report returned by [`compact_brain`] describing what was freed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompactReport {
    /// brain.db file size in bytes before compaction.
    pub bytes_before: u64,
    /// brain.db file size in bytes after compaction (WAL checkpointed first).
    pub bytes_after: u64,
    /// Number of events deleted by `--trim-events-older-than` (0 when not requested).
    pub events_trimmed: u64,
    /// Number of invalidated memory rows purged (0 when not requested).
    pub invalidated_memories_purged: u64,
}

/// Reclaim dead space in brain.db.
///
/// 1. Acquires the project lock (same as `rebuild_projection`).
/// 2. Optionally purges invalidated memory rows (`purge_invalidated`).
/// 3. Optionally trims old events (`trim_events_older_than`).
/// 4. Runs `VACUUM` (outside any transaction) to rebuild the file in-place.
/// 5. Checkpoints the WAL before measuring `bytes_after` so the measurement
///    reflects the on-disk file, not the shadow WAL.
pub fn compact_brain(
    start: &Path,
    trim_events_older_than: Option<std::time::Duration>,
    purge_invalidated: bool,
) -> KimetsuResult<CompactReport> {
    let (paths, _config, conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "brain compact", None)?;

    // Step 2: record bytes_before.
    let bytes_before = fs::metadata(&paths.brain_db).map(|m| m.len()).unwrap_or(0);

    // Step 3: purge invalidated memories (optional, gated by caller).
    let invalidated_memories_purged = if purge_invalidated {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        conn.execute_batch(
            "DELETE FROM memories_fts WHERE memory_id IN (
                 SELECT memory_id FROM memories WHERE invalidated_at IS NOT NULL
             );
             DELETE FROM memories WHERE invalidated_at IS NOT NULL;",
        )?;
        count as u64
    } else {
        0
    };

    // Step 4: trim old events (optional, gated by caller).
    let events_trimmed = if let Some(dur) = trim_events_older_than {
        // Compute the cutoff as an RFC 3339 string (UTC) so it compares
        // correctly against the TEXT `ts` column.
        let cutoff_secs = dur.as_secs();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff_unix = now_unix.saturating_sub(cutoff_secs);
        // Format as a naive UTC RFC 3339 string (matches the stored format).
        let cutoff_rfc3339 = {
            let secs = cutoff_unix as i64;
            // Use the `time` crate (already a dependency of projector.rs).
            use time::OffsetDateTime;
            use time::format_description::well_known::Rfc3339;
            OffsetDateTime::from_unix_timestamp(secs)
                .map_err(|e| format!("compact_brain: invalid cutoff timestamp: {e}"))?
                .format(&Rfc3339)
                .map_err(|e| format!("compact_brain: failed to format cutoff: {e}"))?
        };
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE ts < ?1",
            rusqlite::params![cutoff_rfc3339],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM events WHERE ts < ?1",
            rusqlite::params![cutoff_rfc3339],
        )?;
        count as u64
    } else {
        0
    };

    // Step 5: VACUUM — must run outside any active transaction.
    // `rusqlite::Connection` does not hold an implicit transaction here so
    // execute_batch is safe.
    conn.execute_batch("VACUUM;")?;

    // Step 6: Checkpoint the WAL so bytes_after reflects the real file size
    // (on systems without WAL mode this is a no-op).
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let bytes_after = fs::metadata(&paths.brain_db).map(|m| m.len()).unwrap_or(0);

    Ok(CompactReport {
        bytes_before,
        bytes_after,
        events_trimmed,
        invalidated_memories_purged,
    })
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

    /// v0.8: create an isolated temp project root. A minimal `git init`
    /// gives the dir its own git toplevel so `ProjectPaths::discover`
    /// resolves to THIS dir instead of climbing to an enclosing repo
    /// (e.g. a developer's `$HOME` git repo) — which would otherwise
    /// make parallel tests share one brain.db + project.lock. Without
    /// this, tests pass only when `TMP` points outside any git repo.
    fn test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("kimetsu-test-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    #[test]
    fn w1_5_init_creates_kimetsu_dir_but_no_runs_dir() {
        with_user_brain_disabled(|| {
            let root = test_root();
            let summary = init_project(&root, false).expect("init");
            // The .kimetsu/ dir + brain.db + project.toml are created...
            assert!(summary.kimetsu_dir.exists(), ".kimetsu/ must exist");
            assert!(summary.brain_db.exists(), "brain.db must be created");
            assert!(
                summary.kimetsu_dir.join("project.toml").exists(),
                "project.toml must be written"
            );
            // ...but a fresh init does NOT eagerly create runs/ (it's created
            // lazily only when an agent run needs it).
            assert!(
                !summary.kimetsu_dir.join("runs").exists(),
                "fresh init must NOT create a runs/ dir"
            );
        });
    }

    #[test]
    fn search_memories_paginates_and_filters_by_kind() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "linker link.exe not found on windows",
            )
            .expect("add fp");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Command,
                "run cargo build with the link.exe linker on PATH",
            )
            .expect("add cmd");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "the office plant needs watering on tuesdays",
            )
            .expect("add fact");

            // "linker" matches the two link.exe memories, not the plant fact.
            let hits = search_memories(&root, "linker", 10, 0, None, None).expect("search");
            assert!(hits.len() >= 2, "expected >=2 hits, got {}", hits.len());
            assert!(
                hits.iter()
                    .all(|h| h.text.to_ascii_lowercase().contains("link"))
            );

            // Pagination: two single-row pages return distinct rows.
            let p1 = search_memories(&root, "linker", 1, 0, None, None).expect("p1");
            let p2 = search_memories(&root, "linker", 1, 1, None, None).expect("p2");
            assert_eq!(p1.len(), 1);
            assert_eq!(p2.len(), 1);
            assert_ne!(p1[0].memory_id, p2[0].memory_id, "offset must advance");

            // Kind filter narrows to failure_pattern only.
            let fp =
                search_memories(&root, "linker", 10, 0, Some("failure_pattern"), None).expect("fp");
            assert!(!fp.is_empty());
            assert!(fp.iter().all(|h| h.kind == "failure_pattern"));

            // A query with no FTS tokens returns empty, not an error.
            assert!(
                search_memories(&root, "   ", 10, 0, None, None)
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn reindex_with_explicit_embedder_uses_that_model() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "alpha beta gamma",
            )
            .expect("add");
            // The explicit-embedder path (used by `model set`) must
            // re-embed with the GIVEN embedder, regardless of the
            // process default.
            use crate::embeddings::Embedder as _;
            let stub = crate::embeddings::StubEmbedder::new();
            let report = crate::reindex::reindex_all_with_embedder(
                &root,
                crate::reindex::ReindexOptions {
                    scope: crate::reindex::ReindexScope::Project,
                    dry_run: false,
                    force: false,
                    limit: None,
                },
                &stub,
            )
            .expect("reindex");
            assert_eq!(report.embedder_model_id, stub.model_id());
            assert!(
                report.project.updated >= 1,
                "the row should be re-embedded with the stub model"
            );
        });
    }

    #[test]
    fn retrieve_proactive_returns_actionable_kind_and_excludes_others() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "linker link.exe not found -> run from x64 Native Tools prompt",
            )
            .expect("add fp");
            // A high-overlap FACT that would outrank lexically but is NOT an
            // actionable kind — the kinds filter must drop it.
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "linker link.exe trivia: link.exe ships with MSVC",
            )
            .expect("add fact");

            let request = ContextRequest {
                stage: "localization".to_string(),
                query: "error: linker `link.exe` not found".to_string(),
                budget_tokens: 600,
                min_score: 0.2,
                max_capsules: 1,
                kinds: vec!["failure_pattern".to_string(), "command".to_string()],
                ..Default::default()
            };
            let bundle = retrieve_proactive_readonly(&root, request).expect("proactive");
            assert!(!bundle.skipped, "should surface the failure_pattern");
            assert_eq!(bundle.capsules.len(), 1);
            // The single capsule must be the failure_pattern, not the fact.
            assert!(
                bundle.capsules[0].summary.contains("failure_pattern"),
                "got summary: {}",
                bundle.capsules[0].summary
            );
            assert!(!bundle.capsules[0].summary.contains("trivia"));
        });
    }

    #[test]
    fn memory_add_survives_projection_rebuild_from_trace() {
        with_user_brain_disabled(|| {
            let root = test_root();
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

            let event_count = rebuild_projection(&root, false).expect("rebuild projection");
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

    /// v0.4.5 end-to-end: secrets in `add_memory` text never reach
    /// brain.db. The redacted row keeps the surrounding context so
    /// the memory is still useful — only the credential is scrubbed.
    #[test]
    fn add_memory_redacts_secrets_before_persist() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            let raw = "Add CLAUDE_CODE_OAUTH_TOKEN=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf to .env";
            let memory_id =
                add_memory(&root, MemoryScope::Repo, MemoryKind::Command, raw).expect("add memory");

            let memories = list_memories(&root).expect("list");
            let stored = memories
                .iter()
                .find(|m| m.memory_id == memory_id)
                .expect("memory present");
            assert!(
                !stored.text.contains("sk-ant-api03"),
                "raw secret must NOT survive to brain.db: {}",
                stored.text
            );
            assert!(
                stored.text.contains("[REDACTED:anthropic_oauth]"),
                "placeholder must be present: {}",
                stored.text
            );
            assert!(
                stored.text.contains("CLAUDE_CODE_OAUTH_TOKEN") && stored.text.contains(".env"),
                "non-secret context must be preserved: {}",
                stored.text
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    #[test]
    fn repo_ingest_indexes_searchable_files_and_context_capsules() {
        let root = test_root();
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

        rebuild_projection(&root, false).expect("rebuild projection");
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
            // MP-4a outcome attribution + v0.5.1 citation split:
            // a memory that is BOTH injected (in context.injected) AND
            // cited (via memory.cited from the cite_memory tool) earns
            // the strong +1.0 usefulness delta on run.finished.
            //
            // Per-run counting: the same memory injected into two
            // stages of one run still counts once.
            let root = test_root();
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
                    // v0.5.1: model explicitly cited the memory in
                    // turn 3 — earns the strong +1.0 delta.
                    Event::new(
                        run_id,
                        "memory.cited",
                        serde_json::json!({
                            "memory_id": memory_id,
                            "turn": 3,
                            "rationale": "using rg from memory",
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
            // Flagship 2 / Story 2.1: memory starts with initial_kind_weight = 0.05
            // (Preference) and earns +1.0 strong delta on run.finished → 1.05.
            let expected = 1.0 + 0.05; // 1.0 strong delta + Preference kind weight
            assert!(
                (m.usefulness_score - expected).abs() < 1e-4,
                "expected strong-signal usefulness_score = {expected}, got {}",
                m.usefulness_score
            );

            fs::remove_dir_all(root).expect("remove temp project");
        });
    }

    /// v0.5.1: silent-passenger path. A memory that was retrieved
    /// (in context.injected) but the model never cited gets the
    /// weak +0.1 delta on run.finished, not the full +1.0.
    /// Encourages the model to actually call `cite_memory`.
    #[test]
    fn run_finished_gives_weak_signal_to_silent_passenger_memories() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");
            let memory_id = add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Preference,
                "Silent passenger memory.",
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
                            "memory_ids": [memory_id.clone()],
                            "prior_run_ids": [],
                            "file_paths": [],
                        }),
                    ),
                    // NO memory.cited event for this memory.
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
            assert_eq!(m.use_count, 1);
            // Flagship 2 / Story 2.1: memory starts with initial_kind_weight = 0.05
            // (Preference) and earns +0.1 weak delta on run.finished → 0.15.
            let expected = 0.1 + 0.05; // 0.1 weak delta + Preference kind weight
            assert!(
                (m.usefulness_score - expected).abs() < 1e-4,
                "silent passenger should get +0.1 on top of seed, got {}",
                m.usefulness_score
            );
        });
    }

    /// v0.5.1 end-to-end: `blame_run` walks memory_citations +
    /// context.injected + terminal events and surfaces per-memory
    /// attribution. Cited memories appear under `cited`, retrieved-
    /// but-uncited under `silent_passengers`, and the outcome
    /// reflects the run's terminal event.
    #[test]
    fn blame_run_separates_cited_from_silent_passengers() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");
            let cited_id = add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Preference,
                "prefer ripgrep over grep",
            )
            .expect("add cited");
            let silent_id = add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Convention,
                "use cargo nextest for tests",
            )
            .expect("add silent");

            let run_id = RunId::new();
            {
                let (paths, _config, conn) = load_project(&root).expect("load");
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
                            "memory_ids": [cited_id.clone(), silent_id.clone()],
                            "prior_run_ids": [],
                            "file_paths": [],
                        }),
                    ),
                    Event::new(
                        run_id,
                        "memory.cited",
                        serde_json::json!({
                            "memory_id": cited_id,
                            "turn": 4,
                            "rationale": "used the rg pattern",
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

            let report = blame_run(&root, &run_id.to_string()).expect("blame");
            assert_eq!(report.outcome, "success");
            assert!(report.failure_category.is_none());
            assert_eq!(report.cited.len(), 1, "exactly one cited memory");
            let cited = &report.cited[0];
            assert_eq!(cited.memory_id, cited_id);
            assert_eq!(cited.turn, 4);
            assert_eq!(cited.rationale.as_deref(), Some("used the rg pattern"));
            assert!(cited.text_preview.contains("ripgrep"));

            assert_eq!(report.silent_passengers.len(), 1);
            let silent = &report.silent_passengers[0];
            assert_eq!(silent.memory_id, silent_id);
            assert!(silent.text_preview.contains("nextest"));

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    #[test]
    fn run_failed_decrements_usefulness_unless_gate() {
        // run.failed with category != "Gate" decrements; category == "Gate"
        // is a graceful early-exit (e.g. the plan-create existence guard)
        // and must not blame memories that happened to be in context.
        let root = test_root();
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

            // Second run: real implementation failure + the memory
            // was cited via memory.cited -> -1.0 strong signal.
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
                // v0.5.1: cite the memory so this run earns the
                // strong -1.0 penalty (the brain pushed wrong).
                Event::new(
                    impl_run,
                    "memory.cited",
                    serde_json::json!({
                        "memory_id": memory_id,
                        "turn": 2,
                        "rationale": "trusted the memory's pattern",
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
        // Flagship 2 / Story 2.1: memory starts with initial_kind_weight = 0.15
        // (Convention) and earns -1.0 strong delta on run.failed → -0.85.
        let expected = 0.15 - 1.0; // -1.0 strong delta + Convention kind weight
        assert!(
            (m.usefulness_score - expected).abs() < 1e-4,
            "expected usefulness_score = {expected}, got {}",
            m.usefulness_score
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    #[test]
    fn run_aborted_does_not_update_usefulness() {
        let root = test_root();
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
        // Flagship 2 / Story 2.1: memory starts with initial_kind_weight = 0.15
        // (Convention). run.aborted must NOT change usefulness — only the seed remains.
        let expected_seed = 0.15_f32; // Convention kind weight
        assert!(
            (m.usefulness_score - expected_seed).abs() < 1e-4,
            "expected usefulness_score = {expected_seed} (initial seed only), got {}",
            m.usefulness_score
        );

        fs::remove_dir_all(root).expect("remove temp project");
    }

    #[test]
    fn list_proposals_filters_and_reject_records_reason() {
        let root = test_root();
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
        let root = test_root();
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

        // Rebuild from table and confirm invalidation survives.
        rebuild_projection(&root, false).expect("rebuild projection");
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
            let root = test_root();
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

            // The row itself still exists in brain.db (S4.4: list_memories now
            // filters invalidated rows, matching user-brain behaviour, so we
            // verify persistence via a direct DB query instead).
            {
                let (_paths2, _config2, conn2) = load_project(&root).expect("load for check");
                let still_there: i64 = conn2
                    .query_row(
                        "SELECT COUNT(*) FROM memories WHERE memory_id = ?1",
                        rusqlite::params![&memory_id],
                        |row| row.get(0),
                    )
                    .expect("db query");
                assert_eq!(still_there, 1, "invalidated row must persist in brain.db");
            } // conn2 / _paths2 dropped here — Windows file lock released
            // But list_memories must NOT surface it (active-only since S4.4).
            let active = list_memories(&root).expect("list after invalidation");
            assert!(
                active.iter().all(|m| m.memory_id != memory_id),
                "invalidated memory must not appear in list_memories"
            );

            fs::remove_dir_all(root).expect("remove temp project");
        });
    }

    /// MP-6: `list_memories_top` returns invalidated_at IS NULL memories
    /// sorted by ratio descending, filtered by `min_uses`. Memories with
    /// use_count below the threshold are dropped entirely so the listing
    /// only shows entries the broker bias actually applies to.
    #[test]
    fn list_memories_top_sorts_by_usefulness_ratio_and_drops_small_samples() {
        let root = test_root();
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
        let root = test_root();
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
        let root = test_root();
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

    /// End-to-end regression for the add -> list_conflicts ->
    /// resolve_conflict plumbing. It must be AGNOSTIC to which
    /// embedder backs the build: `cargo test --workspace`
    /// feature-unifies `embeddings` into this crate (kimetsu-cli
    /// enables `kimetsu-brain/embeddings`), so
    /// `open_default_embedder()` returns the real fastembed model
    /// here, not the noop. The two memories below are therefore on
    /// unrelated topics: cosine stays well under the 0.82 conflict
    /// threshold for any real embedder, and the noop build trivially
    /// records zero -- so `list_conflicts` is deterministically empty
    /// either way.
    ///
    /// Real near-duplicate semantic detection is exercised
    /// exhaustively in `crate::conflict::tests` with a StubEmbedder;
    /// this test guards the project-level plumbing only.
    #[test]
    fn add_memory_distinct_texts_no_conflicts() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            // Two memories on unrelated topics: neither the noop nor
            // a real embedder flags them as conflicting (cosine well
            // under the 0.82 threshold), and they don't collide via
            // the exact-text dedup gate, so both rows simply coexist.
            let _m1 = add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Preference,
                "Prefer thiserror for library error types.",
            )
            .expect("add m1");
            let _m2 = add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Preference,
                "Cache HTTP responses with a one-hour TTL.",
            )
            .expect("add m2");

            let open = list_conflicts(&root, 50).expect("list_conflicts");
            assert!(
                open.is_empty(),
                "distinct-topic memories must not conflict; got {} rows",
                open.len()
            );

            // Resolving a non-existent id should return false, not error.
            let resolved = resolve_conflict(&root, "does-not-exist", "kept_both")
                .expect("resolve_conflict on unknown id");
            assert!(!resolved, "unknown conflict id should resolve to false");

            // Invalid resolution strings should be rejected up front.
            let err = resolve_conflict(&root, "does-not-exist", "garbage")
                .expect_err("invalid resolution should error");
            assert!(format!("{err}").contains("invalid conflict resolution"));

            fs::remove_dir_all(root).expect("remove temp project");
        });
    }

    /// A1: project.toml load gate is keyed to KIMETSU_CONFIG_VERSION, not
    /// KIMETSU_SCHEMA_VERSION. A config with schema_version =
    /// KIMETSU_CONFIG_VERSION + 1 must be REJECTED by load_project, proving
    /// the gate is active and uses the config constant (not the DB constant).
    /// When both constants are 1 this also demonstrates that the value of 1
    /// is the correct expected value.
    #[test]
    fn load_project_rejects_future_config_version() {
        with_user_brain_disabled(|| {
            use kimetsu_core::KIMETSU_CONFIG_VERSION;
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            // Write a project.toml with an unsupported (future) config version.
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root)
                .expect("discover paths after git_init_boundary");
            fs::create_dir_all(&paths.kimetsu_dir).expect("create .kimetsu dir");
            let bad_version = KIMETSU_CONFIG_VERSION + 1;
            let toml_str = format!(
                r#"
[kimetsu]
project_id = "test-config-gate"
schema_version = {bad_version}

[model]
provider = "anthropic"
model = "claude-opus-4-7"
api_key_env = "ANTHROPIC_API_KEY"
max_output_tokens = 8192
temperature = 0.2
request_timeout_secs = 120

[broker]
default_budget_tokens = 6000

[broker.weights]
relevance = 0.5
confidence = 0.2
freshness = 0.2
scope = 0.1

[shell]
default_timeout_secs = 60
max_timeout_secs = 600
env_allowlist_extra = []
redact_secrets = true

[ingestion]
max_file_bytes = 524288
extra_skip_dirs = []
max_total_files = 50000

[run]
max_total_tool_calls = 60
max_total_model_turns = 30
max_total_cost_usd = 250.0
"#
            );
            fs::write(&paths.project_toml, &toml_str).expect("write bad project.toml");
            let err = load_project(&root).expect_err("future config version must be rejected");
            let msg = format!("{err}");
            assert!(
                msg.contains(&bad_version.to_string()),
                "error message should mention the bad version; got: {msg}"
            );
            assert!(
                msg.contains(&KIMETSU_CONFIG_VERSION.to_string()),
                "error message should mention the expected version; got: {msg}"
            );
            fs::remove_dir_all(root).expect("remove temp project");
        });
    }

    // ── D2: abort_run ──────────────────────────────────────────────────────────

    /// Helper: create a dangling run (run.started only, no terminal event).
    fn make_dangling_run(root: &std::path::Path) -> RunId {
        let (paths, _config, conn) = load_project(root).expect("load project");
        let run_id = RunId::new();
        let (mut writer, _) = TraceWriter::create(&paths, run_id).expect("create trace");
        let started = Event::new(
            run_id,
            "run.started",
            serde_json::json!({"project_id": "test", "task": "dangling task"}),
        );
        writer.append(&started, true).expect("append started");
        projector::apply_events(&conn, &[started]).expect("project started");
        run_id
    }

    #[test]
    fn abort_run_stamps_aborted_and_frees_lock() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");

            let run_id = make_dangling_run(&root);

            // Abort it.
            abort_run(&root, &run_id.to_string()).expect("abort_run");

            // The run should now have terminal_kind = "run.aborted".
            let run = show_run(&root, &run_id.to_string())
                .expect("show_run")
                .expect("run exists");
            assert_eq!(
                run.terminal_kind.as_deref(),
                Some("run.aborted"),
                "terminal_kind should be run.aborted"
            );

            // Lock should be absent (clear_force ran).
            let paths = kimetsu_core::paths::ProjectPaths::discover(&root).expect("paths");
            assert!(
                !paths.lock_file.exists(),
                "lock file should not exist after abort"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    #[test]
    fn abort_run_already_finished_returns_err() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");

            // Use add_memory which creates a run.started + run.finished.
            add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "some fact")
                .expect("add memory");

            let runs = list_runs(&root).expect("list runs");
            assert!(!runs.is_empty(), "should have at least one run");
            let finished_run = runs
                .iter()
                .find(|r| r.terminal_kind.is_some())
                .expect("should have a finished run");

            let err = abort_run(&root, &finished_run.run_id)
                .expect_err("aborting a finished run should error");
            let msg = format!("{err}");
            assert!(
                msg.contains("already terminal"),
                "error should mention 'already terminal', got: {msg}"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    #[test]
    fn abort_run_unknown_id_returns_err() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("mkdir");
            init_project(&root, false).expect("init");

            let fake_id = RunId::new().to_string();
            let err = abort_run(&root, &fake_id).expect_err("aborting an unknown run should error");
            let msg = format!("{err}");
            assert!(
                msg.contains("unknown run_id"),
                "error should mention 'unknown run_id', got: {msg}"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    // ── W1.3 tests ────────────────────────────────────────────────────────────

    /// W1.3 normal path: add memories (events land in DB), wipe the derived
    /// tables, call rebuild_projection(false) — it replays the events table
    /// in-place and restores the memories without touching the events rows.
    #[test]
    fn rebuild_from_events_table_restores_memories() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            let id1 = add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Convention,
                "W1.3: prefer explicit error types over anyhow in library crates",
            )
            .expect("add memory 1");
            let id2 = add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Command,
                "W1.3: run cargo fmt --all before committing",
            )
            .expect("add memory 2");

            // Wipe the derived tables — events table stays intact.
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute_batch("DELETE FROM memories; DELETE FROM memories_fts;")
                    .expect("wipe derived tables");
            }

            // Sanity: memories are gone.
            let gone = list_memories(&root).expect("list after wipe");
            assert_eq!(gone.len(), 0, "derived tables should be empty after wipe");

            // Rebuild from the events table (normal path, from_traces = false).
            let count = rebuild_projection(&root, false).expect("rebuild_projection");
            assert!(
                count > 0,
                "should have replayed at least one event; got {count}"
            );

            // Both memories must be restored.
            let restored = list_memories(&root).expect("list after rebuild");
            assert_eq!(
                restored.len(),
                2,
                "both memories should be restored after rebuild; got {:?}",
                restored.iter().map(|m| &m.memory_id).collect::<Vec<_>>()
            );
            let ids: Vec<_> = restored.iter().map(|m| m.memory_id.clone()).collect();
            assert!(ids.contains(&id1), "id1 must be restored");
            assert!(ids.contains(&id2), "id2 must be restored");

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    /// W1.3 --from-traces path: manually write a trace.jsonl on disk (simulating
    /// a legacy run that pre-dates W1.4, when memory ops did write trace files),
    /// wipe the events table and derived tables, then call rebuild_projection(true)
    /// — it must re-import from the on-disk trace file and restore the memory.
    ///
    /// W1.4 note: add_memory no longer writes trace files, so this test creates
    /// the trace file directly via TraceWriter (the same way agent runs still do).
    /// This keeps the --from-traces code-path exercised for genuine legacy traces.
    #[test]
    fn rebuild_from_traces_flag_reimports_on_disk_traces() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            // Build a legacy trace.jsonl directly — simulates what add_memory
            // wrote before W1.4. This keeps --from-traces coverage alive for
            // genuine legacy brain directories that still have trace files.
            let memory_id = Ulid::new().to_string();
            let run_id = RunId::new();
            {
                let (paths, config, conn) = load_project(&root).expect("load");
                let (mut writer, _run_paths) =
                    TraceWriter::create(&paths, run_id).expect("trace writer");
                let text = "W1.3: from_traces re-imports events from trace.jsonl files";
                let normalized = kimetsu_core::memory::normalize_memory_text(text);
                let evs: Vec<Event> = vec![
                    admin_started_event(&paths, &config, run_id, "memory add").expect("started"),
                    Event::new(
                        run_id,
                        "memory.accepted",
                        serde_json::json!({
                            "proposal_id": null,
                            "memory_id": memory_id,
                            "scope": "repo",
                            "kind": "fact",
                            "text": text,
                            "normalized_text": normalized,
                            "confidence": 1.0,
                            "provenance_snapshot": {
                                "source": "manual_cli",
                                "run_id": run_id.to_string(),
                                "text": text,
                            }
                        }),
                    ),
                    admin_finished_event(run_id),
                ];
                for ev in &evs {
                    writer.append(ev, true).expect("append");
                }
                // Also persist to events table so the memory shows up now.
                projector::apply_events(&conn, &evs).expect("apply");
            }

            // Confirm memory is present.
            let initial = list_memories(&root).expect("list initial");
            assert_eq!(initial.len(), 1);

            // Wipe both events table AND derived tables to simulate a fully
            // blank DB that still has trace.jsonl files on disk.
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute_batch(
                    "DELETE FROM events; DELETE FROM memories; DELETE FROM memories_fts;",
                )
                .expect("wipe events + derived tables");
            }

            // rebuild_projection with from_traces = true must re-import.
            let count = rebuild_projection(&root, true).expect("rebuild_projection --from-traces");
            assert!(
                count > 0,
                "should have imported ≥1 event from on-disk traces; got {count}"
            );

            let restored = list_memories(&root).expect("list after trace import");
            assert_eq!(
                restored.len(),
                1,
                "memory must be restored from on-disk traces"
            );
            assert_eq!(restored[0].memory_id, memory_id);

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    /// W1.3 auto-fallback: manually write a trace.jsonl (simulating a legacy run),
    /// wipe the events table and derived tables to simulate a pre-W1.1 state, then
    /// call rebuild_projection(false). The auto-fallback detects the empty events
    /// table, finds the on-disk traces, and imports them automatically.
    ///
    /// W1.4 note: add_memory no longer writes trace files, so the trace is created
    /// directly via TraceWriter — the same pattern a real legacy brain would have.
    #[test]
    fn rebuild_auto_fallback_imports_traces_when_events_table_empty() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            // Write a legacy trace.jsonl directly to simulate a pre-W1.4 brain.
            let memory_id = Ulid::new().to_string();
            let run_id = RunId::new();
            {
                let (paths, config, conn) = load_project(&root).expect("load");
                let (mut writer, _run_paths) =
                    TraceWriter::create(&paths, run_id).expect("trace writer");
                let text = "W1.3: auto-fallback recovers from pre-W1.1 events wipe";
                let normalized = kimetsu_core::memory::normalize_memory_text(text);
                let evs: Vec<Event> = vec![
                    admin_started_event(&paths, &config, run_id, "memory add").expect("started"),
                    Event::new(
                        run_id,
                        "memory.accepted",
                        serde_json::json!({
                            "proposal_id": null,
                            "memory_id": memory_id,
                            "scope": "repo",
                            "kind": "convention",
                            "text": text,
                            "normalized_text": normalized,
                            "confidence": 1.0,
                            "provenance_snapshot": {
                                "source": "manual_cli",
                                "run_id": run_id.to_string(),
                                "text": text,
                            }
                        }),
                    ),
                    admin_finished_event(run_id),
                ];
                for ev in &evs {
                    writer.append(ev, true).expect("append");
                }
                // Persist to events table (simulates a post-W1.1 add, pre-W1.4).
                projector::apply_events(&conn, &evs).expect("apply");
            }

            // Simulate a pre-W1.1 rebuild that wiped the events table.
            // Leave the trace.jsonl files intact.
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute_batch(
                    "DELETE FROM events; DELETE FROM memories; DELETE FROM memories_fts;",
                )
                .expect("simulate pre-W1.1 wipe");
            }

            // Call rebuild with from_traces = false; the auto-fallback should
            // detect the empty events table and import from traces.
            let count = rebuild_projection(&root, false).expect("rebuild_projection auto-fallback");
            assert!(
                count > 0,
                "auto-fallback should have imported ≥1 event from traces; got {count}"
            );

            let restored = list_memories(&root).expect("list after auto-fallback");
            assert_eq!(
                restored.len(),
                1,
                "auto-fallback must restore memory from traces when events table was empty"
            );
            assert_eq!(restored[0].memory_id, memory_id);

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    // ── W1.4 tests ────────────────────────────────────────────────────────────

    /// Helper: count subdirectories of `runs_dir` (each subdir is a run dir).
    fn run_subdir_count(runs_dir: &std::path::Path) -> usize {
        if !runs_dir.exists() {
            return 0;
        }
        fs::read_dir(runs_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .count()
            })
            .unwrap_or(0)
    }

    /// W1.4: add_memory creates no on-disk run dir, but the memory is present
    /// and the runs TABLE row exists (so blame still works).
    #[test]
    fn w1_4_add_memory_creates_no_run_dir_but_memory_and_runs_row_exist() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            // Derive runs_dir without holding a connection open across the test.
            let runs_dir = {
                let paths =
                    kimetsu_core::paths::ProjectPaths::discover(&root).expect("discover paths");
                paths.runs_dir.clone()
            };
            let before = run_subdir_count(&runs_dir);

            let memory_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "W1.4: no run dir should be created for memory writes",
            )
            .expect("add memory");

            // (a) No new run subdir on disk.
            let after = run_subdir_count(&runs_dir);
            assert_eq!(
                after, before,
                "add_memory must not create a runs/<id>/ directory (before={before}, after={after})"
            );

            // (b) Memory is listed.
            let memories = list_memories(&root).expect("list");
            assert!(
                memories.iter().any(|m| m.memory_id == memory_id),
                "memory must be present after add_memory"
            );

            // (c) The runs TABLE row exists (projector created it from run.started).
            let runs_count: i64 = {
                let (_paths, _config, conn) = load_project(&root).expect("load for runs check");
                conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
                    .expect("count runs")
            };
            assert!(
                runs_count >= 1,
                "projector must have inserted a runs row from the run.started event (got {runs_count})"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    /// W1.4: memory survives rebuild_projection(false) without any trace file
    /// — proving events landed in the durable table.
    #[test]
    fn w1_4_memory_survives_rebuild_from_events_table_no_trace() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            let memory_id = add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Convention,
                "W1.4: events are durable without a trace file",
            )
            .expect("add memory");

            // Wipe derived tables (leave events table).
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute_batch("DELETE FROM memories; DELETE FROM memories_fts;")
                    .expect("wipe derived tables");
            }

            // rebuild_projection(false) uses the events table — no trace files needed.
            let count = rebuild_projection(&root, false).expect("rebuild");
            assert!(count > 0, "should have replayed events; got {count}");

            let restored = list_memories(&root).expect("list after rebuild");
            assert!(
                restored.iter().any(|m| m.memory_id == memory_id),
                "memory must survive rebuild from events table without a trace file"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    /// W1.4: propose_memory, ingest_repo, invalidate_memory, and reject_proposal
    /// likewise create no new on-disk run subdirectory.
    #[test]
    fn w1_4_memory_ops_create_no_run_dirs() {
        with_user_brain_disabled(|| {
            let root = test_root();
            // ingest_repo needs a real git repo with at least one file.
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"w14-fixture\"\nversion = \"0.1.0\"\n",
            )
            .expect("write Cargo.toml");
            init_project(&root, false).expect("init project");

            // Derive runs_dir without holding a connection open across the test.
            let runs_dir = {
                let paths =
                    kimetsu_core::paths::ProjectPaths::discover(&root).expect("discover paths");
                paths.runs_dir.clone()
            };

            // propose_memory — creates no dir.
            let before = run_subdir_count(&runs_dir);
            let proposal_id = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "W1.4 propose: no run dir",
                0.5,
                "test rationale",
            )
            .expect("propose");
            assert_eq!(
                run_subdir_count(&runs_dir),
                before,
                "propose_memory must not create a run dir"
            );

            // ingest_repo — creates no dir.
            let before = run_subdir_count(&runs_dir);
            ingest_repo(&root).expect("ingest");
            assert_eq!(
                run_subdir_count(&runs_dir),
                before,
                "ingest_repo must not create a run dir"
            );

            // reject_proposal — creates no dir.
            let before = run_subdir_count(&runs_dir);
            reject_proposal(&root, &proposal_id, Some("W1.4 test")).expect("reject");
            assert_eq!(
                run_subdir_count(&runs_dir),
                before,
                "reject_proposal must not create a run dir"
            );

            // invalidate_memory: add a real memory first, then invalidate it.
            let mem_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Command,
                "W1.4 invalidate: no run dir",
            )
            .expect("add");
            let before = run_subdir_count(&runs_dir);
            invalidate_memory(&root, &mem_id, Some("W1.4 test")).expect("invalidate");
            assert_eq!(
                run_subdir_count(&runs_dir),
                before,
                "invalidate_memory must not create a run dir"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    /// W1.4: dedup hit (second identical add_memory call) creates no orphan run dir.
    #[test]
    fn w1_4_dedup_hit_creates_no_orphan_run_dir() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            // Derive runs_dir without holding a connection open across the test.
            let runs_dir = {
                let paths =
                    kimetsu_core::paths::ProjectPaths::discover(&root).expect("discover paths");
                paths.runs_dir.clone()
            };

            // First call: accepted.
            let id1 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "W1.4 dedup: identical text",
            )
            .expect("first add");

            // Both calls produce 0 run dirs total (first also creates none).
            let after_first = run_subdir_count(&runs_dir);
            assert_eq!(after_first, 0, "first add must create no run dir");

            // Second call: dedup hit, returns the same id immediately.
            let id2 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "W1.4 dedup: identical text",
            )
            .expect("second add");

            assert_eq!(id1, id2, "dedup must return the same memory_id");
            let after_second = run_subdir_count(&runs_dir);
            assert_eq!(
                after_second, 0,
                "dedup hit must not create an orphan run dir"
            );

            fs::remove_dir_all(root).expect("cleanup");
        });
    }

    // ── W3.1: runtime wiring tests ─────────────────────────────────────────

    /// W3.1: `open_embedder_for(false)` always returns a noop; `open_embedder_for(true)`
    /// matches `open_default_embedder().is_noop()`. Validates the resolver logic
    /// independently of disk I/O.
    #[test]
    fn w3_1_open_embedder_for_resolver() {
        use crate::embeddings;
        use crate::user_brain::test_env_lock;

        let _guard = test_env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
        // Ensure env is unset so config governs.
        unsafe {
            std::env::remove_var("KIMETSU_BRAIN_EMBEDDER");
        }

        // config=false → always noop.
        assert!(
            embeddings::open_embedder_for(false).is_noop(),
            "open_embedder_for(false) must return a noop embedder"
        );
        // config=true → same as open_default_embedder (noop on lean, real on embeddings build).
        assert_eq!(
            embeddings::open_embedder_for(true).is_noop(),
            embeddings::open_default_embedder().is_noop(),
            "open_embedder_for(true) must match open_default_embedder().is_noop()"
        );

        // Env disable overrides config=true.
        unsafe {
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
        }
        assert!(
            embeddings::open_embedder_for(true).is_noop(),
            "KIMETSU_BRAIN_EMBEDDER=noop must override config=true → noop"
        );

        // Restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
            }
        }
    }

    /// W3.1: `[embedder] enabled = false` in project.toml must result in
    /// NULL embedding column after `add_memory`.
    #[test]
    fn w3_1_config_disabled_writes_null_embedding() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Flip embedder.enabled to false in project.toml.
            let (paths, mut config, _conn) = load_project(&root).expect("load");
            config.embedder.enabled = false;
            let toml = config.to_toml().expect("serialize");
            // Drop _conn before writing toml to release any WAL lock.
            drop(_conn);
            fs::write(&paths.project_toml, toml).expect("write project.toml");

            let memory_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "w3.1 write-disabled: embedder disabled via config",
            )
            .expect("add memory");

            // Assert the embedding column is NULL — no vector was written.
            let embedding: Option<Vec<u8>> = {
                let (_, _, conn) = load_project_readonly(&root).expect("reload");
                let val = conn
                    .query_row(
                        "SELECT embedding FROM memories WHERE memory_id = ?1",
                        rusqlite::params![memory_id],
                        |row| row.get(0),
                    )
                    .expect("query embedding");
                drop(conn);
                val
            };
            assert!(
                embedding.is_none(),
                "embedding must be NULL when [embedder] enabled = false"
            );

            fs::remove_dir_all(root).ok(); // best-effort on Windows
        });
    }

    /// W3.1: `[embedder] enabled = true` (default) does not regress —
    /// on the lean build (no `embeddings` feature) the column is still
    /// NULL (NoopEmbedder); on the embeddings build it would be non-NULL.
    /// This test stays build-agnostic: it just asserts `open_embedder_for(true)`
    /// matches the default embedder's noop status.
    #[test]
    fn w3_1_config_enabled_default_does_not_regress() {
        use crate::embeddings;
        // The lean build returns noop for both paths; embeddings build
        // returns a real embedder for both. Either way they must match.
        let e_default = embeddings::open_default_embedder();
        let e_config = embeddings::open_embedder_for(true);
        assert_eq!(
            e_default.is_noop(),
            e_config.is_noop(),
            "open_embedder_for(true) and open_default_embedder() must have identical noop status"
        );
    }

    /// W3.1: retrieval with `[embedder] enabled = false` still returns
    /// FTS matches and does not panic (FTS-only path is taken).
    #[test]
    fn w3_1_retrieval_fts_only_when_embedder_disabled() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Write a memory with default config (embedder enabled=true on this call).
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "the quick brown fox jumps over the lazy dog",
            )
            .expect("add memory");

            // Now disable the embedder in config.
            let (paths, mut config, _) = load_project(&root).expect("load");
            config.embedder.enabled = false;
            let toml = config.to_toml().expect("serialize");
            fs::write(&paths.project_toml, toml).expect("write project.toml");

            // Retrieval must return something (FTS still works) and must not panic.
            // Wrap in a block so the session (and its Connection) drops before cleanup.
            {
                let session = BrainSession::open_readonly(&root).expect("open readonly");
                let bundle = session
                    .retrieve_context_with_request(crate::context::ContextRequest {
                        stage: "localization".to_string(),
                        query: "fox jumps".to_string(),
                        budget_tokens: 4096,
                        ..Default::default()
                    })
                    .expect("retrieve");
                // FTS should have returned the memory we added.
                // Even if the FTS index is empty (no tokens match), it must not error.
                // The memory text "fox jumps" overlaps with the query — FTS should hit it.
                let _ = bundle; // just assert no panic / error
            }

            fs::remove_dir_all(root).ok(); // best-effort on Windows
        });
    }

    /// v1.0.0: the `UserPromptSubmit` context-hook runs in a throwaway
    /// per-prompt process, so it must NOT load the semantic embedding
    /// model (cold ONNX load can blow the host's 30s hook timeout).
    /// `retrieve_context_lexical` pins the NoopEmbedder so the hook stays
    /// FTS-only and fast regardless of build flavor or `[embedder] enabled`.
    /// This test proves the lexical path still returns FTS matches.
    #[test]
    fn retrieve_context_lexical_returns_fts_hits_without_embedder() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let memory_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Convention,
                "Run zylophonecheck before finalizing the deployment pipeline.",
            )
            .expect("add memory");

            {
                let session = BrainSession::open_readonly(&root).expect("open readonly");
                let bundle = session
                    .retrieve_context_lexical(crate::context::ContextRequest {
                        stage: "localization".to_string(),
                        query: "zylophonecheck deployment pipeline".to_string(),
                        budget_tokens: 4096,
                        ..Default::default()
                    })
                    .expect("retrieve lexical");

                assert!(
                    bundle
                        .capsules
                        .iter()
                        .any(|c| c.expansion_handle == format!("memory:{memory_id}")),
                    "FTS-only lexical retrieval must surface the seeded memory; \
                     got handles: {:?}",
                    bundle
                        .capsules
                        .iter()
                        .map(|c| &c.expansion_handle)
                        .collect::<Vec<_>>()
                );
            }

            fs::remove_dir_all(root).ok(); // best-effort on Windows
        });
    }

    /// v1.0.0: `retrieve_context_with_injected_embedder` must honour the
    /// caller-supplied embedder and still surface FTS matches (NoopEmbedder
    /// path). This is the API the warm embedder daemon will call so it can
    /// reuse a long-lived embedding model across requests.
    #[test]
    fn retrieve_with_injected_embedder_returns_fts_hits() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let memory_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "the distiller harvests lessons at session end",
            )
            .expect("add");

            {
                let session = BrainSession::open_readonly(&root).expect("open ro");
                let bundle = session
                    .retrieve_context_with_injected_embedder(
                        crate::context::ContextRequest {
                            stage: "localization".to_string(),
                            query: "how does the distiller work".to_string(),
                            budget_tokens: 2000,
                            ..Default::default()
                        },
                        &crate::embeddings::NoopEmbedder,
                    )
                    .expect("retrieve");
                assert!(
                    bundle
                        .capsules
                        .iter()
                        .any(|c| c.expansion_handle == format!("memory:{memory_id}")),
                    "FTS path via injected embedder must surface the memory; \
                     got handles: {:?}",
                    bundle
                        .capsules
                        .iter()
                        .map(|c| &c.expansion_handle)
                        .collect::<Vec<_>>()
                );
            }

            fs::remove_dir_all(root).ok(); // best-effort on Windows
        });
    }

    // ── P0 regression tests: GlobalUser add_memory must not require a project ─

    /// Helper: run `f` with the user brain pointed at `dir`, under the
    /// process-wide env lock. Restores env when done and returns `f`'s value.
    fn with_user_brain_at_p0<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        use crate::user_brain::test_env_lock;
        let _guard = test_env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        let prev_en = std::env::var("KIMETSU_USER_BRAIN").ok();
        // SAFETY: scoped by the shared mutex.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", dir);
            std::env::remove_var("KIMETSU_USER_BRAIN");
        }
        let out = f();
        unsafe {
            match prev_dir {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_en {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN"),
            }
        }
        out
    }

    /// P0 regression: `add_memory` with `scope = GlobalUser` from a NON-project
    /// temp dir (no `.kimetsu/project.toml`) must succeed and land in the user
    /// brain. This is the exact scenario the global distiller hits.
    #[test]
    fn p0_global_user_add_memory_works_from_non_project_dir() {
        use crate::user_brain::{list_user_memories, open_user_brain_readonly};

        let user_brain_dir =
            std::env::temp_dir().join(format!("kimetsu-p0-ubrain-{}", Ulid::new()));
        fs::create_dir_all(&user_brain_dir).expect("create user brain dir");

        // `start` is a plain temp dir — NOT a kimetsu project.
        let non_project_dir =
            std::env::temp_dir().join(format!("kimetsu-p0-nonproj-{}", Ulid::new()));
        fs::create_dir_all(&non_project_dir).expect("create non-project dir");

        with_user_brain_at_p0(&user_brain_dir, || {
            add_memory(
                &non_project_dir,
                MemoryScope::GlobalUser,
                MemoryKind::Fact,
                "P0 regression: GlobalUser write from non-project dir",
            )
            .expect("P0: add_memory(GlobalUser) from a non-project dir must succeed");

            // Verify the memory landed in the user brain.
            let conn = open_user_brain_readonly()
                .expect("open ok")
                .expect("user brain must exist after write");
            let mems = list_user_memories(&conn).expect("list");
            assert!(
                mems.iter().any(|m| m
                    .text
                    .contains("P0 regression: GlobalUser write from non-project dir")),
                "P0: the GlobalUser memory must land in the user brain"
            );
        });

        fs::remove_dir_all(&non_project_dir).ok();
        fs::remove_dir_all(&user_brain_dir).ok();
    }

    /// W3.3 toggle preserved: when `start` IS a project with
    /// `[kimetsu] use_user_brain = false`, a GlobalUser `add_memory`
    /// must NOT write to the user brain (falls through to project DB).
    #[test]
    fn p0_global_user_honors_use_user_brain_false_when_start_is_project() {
        use crate::user_brain::{list_user_memories, open_user_brain_readonly};

        // User brain dir: a dedicated temp location so we can assert nothing was written.
        let user_brain_dir =
            std::env::temp_dir().join(format!("kimetsu-p0-w3-ubrain-{}", Ulid::new()));
        fs::create_dir_all(&user_brain_dir).expect("create user brain dir");

        // Create a real kimetsu project.
        let root = test_root();
        init_project(&root, false).expect("init project");

        // Flip use_user_brain = false.
        {
            let (paths, mut config, _) = load_project(&root).expect("load project");
            config.kimetsu.use_user_brain = false;
            let toml = config.to_toml().expect("serialize");
            fs::write(&paths.project_toml, toml).expect("write project.toml");
        }

        let mem_id = with_user_brain_at_p0(&user_brain_dir, || {
            // Write GlobalUser memory — user brain disabled by config → falls through
            // to project DB.
            let id = add_memory(
                &root,
                MemoryScope::GlobalUser,
                MemoryKind::Fact,
                "W3.3 toggle: this must stay in the project DB",
            )
            .expect("add_memory must succeed (falls through to project DB)");

            // Assert user brain was NOT written to within the same env scope.
            let user_conn_opt = open_user_brain_readonly().expect("open ok");
            let user_mems_count = user_conn_opt
                .map(|c| list_user_memories(&c).unwrap_or_default().len())
                .unwrap_or(0);
            assert_eq!(
                user_mems_count, 0,
                "W3.3 toggle: user brain must be empty when use_user_brain=false"
            );
            id
        });

        // Assert 1: memory is in the PROJECT db.
        let project_mems = list_memories(&root).expect("list project memories");
        assert!(
            project_mems.iter().any(|m| m.memory_id == mem_id),
            "W3.3 toggle: memory must be in the project DB when use_user_brain=false"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&user_brain_dir).ok();
    }

    // ── Q5: export / import tests ─────────────────────────────────────────────

    /// Round-trip: add memories to project A, export, parse JSON, import into
    /// project B → `list_memories` on B contains all the texts.
    #[test]
    fn export_import_round_trip() {
        with_user_brain_disabled(|| {
            // --- project A: seed memories --------------------------------
            let root_a = test_root();
            init_project(&root_a, false).expect("init A");
            add_memory(
                &root_a,
                MemoryScope::Project,
                MemoryKind::Fact,
                "alpha fact",
            )
            .expect("add fact");
            add_memory(
                &root_a,
                MemoryScope::Project,
                MemoryKind::Convention,
                "beta convention",
            )
            .expect("add conv");
            add_memory(
                &root_a,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "gamma failure",
            )
            .expect("add fp");

            // Export
            let (exported, _scrub) =
                export_memories(&root_a, None, None, false, false).expect("export");
            assert_eq!(exported.len(), 3, "must export all 3 active memories");

            // All fields present
            for e in &exported {
                assert!(!e.text.is_empty());
                assert!(!e.scope.is_empty());
                assert!(!e.kind.is_empty());
            }

            // Serialize → parse (tests the JSON round-trip)
            let json = serde_json::to_string_pretty(&exported).expect("serialize");
            let parsed: Vec<MemoryExport> = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed.len(), 3);

            // --- project B: import and verify ----------------------------
            let root_b = test_root();
            init_project(&root_b, false).expect("init B");

            let summary = import_memories(&root_b, &parsed, None).expect("import");
            assert_eq!(
                summary.imported, 3,
                "all 3 must be imported into the empty project B"
            );
            assert_eq!(summary.deduped, 0, "no duplicates expected on first import");

            let mems_b = list_memories(&root_b).expect("list B");
            let texts_b: Vec<&str> = mems_b.iter().map(|m| m.text.as_str()).collect();
            assert!(
                texts_b.contains(&"alpha fact"),
                "alpha fact missing from B: {texts_b:?}"
            );
            assert!(
                texts_b.contains(&"beta convention"),
                "beta convention missing from B: {texts_b:?}"
            );
            assert!(
                texts_b.contains(&"gamma failure"),
                "gamma failure missing from B: {texts_b:?}"
            );

            fs::remove_dir_all(&root_a).ok();
            fs::remove_dir_all(&root_b).ok();
        });
    }

    /// Filter: `export_memories(Some(Project), Some(FailurePattern))` returns
    /// only memories matching both the scope AND the kind filter.
    #[test]
    fn export_scope_kind_filter() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "fp1",
            )
            .expect("add fp1");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "fp2",
            )
            .expect("add fp2");
            add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "fact1").expect("add fact");
            add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::FailurePattern,
                "repo-fp",
            )
            .expect("add repo-fp");

            // Filter: project scope + failure_pattern kind
            let (filtered, _) = export_memories(
                &root,
                Some(MemoryScope::Project),
                Some(MemoryKind::FailurePattern),
                false,
                false,
            )
            .expect("export filtered");
            assert_eq!(
                filtered.len(),
                2,
                "must return only the 2 project-scope failure_patterns, got: {filtered:?}"
            );
            assert!(filtered.iter().all(|e| e.scope == "project"));
            assert!(filtered.iter().all(|e| e.kind == "failure_pattern"));

            // Scope-only filter: all project memories
            let (scope_only, _) =
                export_memories(&root, Some(MemoryScope::Project), None, false, false)
                    .expect("scope filter");
            assert_eq!(scope_only.len(), 3, "3 project-scope memories total");

            // Kind-only filter: all failure_patterns (project + repo)
            let (kind_only, _) =
                export_memories(&root, None, Some(MemoryKind::FailurePattern), false, false)
                    .expect("kind filter");
            assert_eq!(
                kind_only.len(),
                3,
                "3 failure_patterns total (2 project + 1 repo)"
            );

            fs::remove_dir_all(&root).ok();
        });
    }

    /// Dedup: importing the same set twice into one project → second import
    /// reports all entries as deduped; `list_memories` count is unchanged.
    #[test]
    fn import_dedup_on_second_import() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let entries = vec![
                MemoryExport {
                    text: "dedup alpha".to_string(),
                    scope: "project".to_string(),
                    kind: "fact".to_string(),
                    confidence: 1.0,
                    created_at: None,
                },
                MemoryExport {
                    text: "dedup beta".to_string(),
                    scope: "project".to_string(),
                    kind: "convention".to_string(),
                    confidence: 1.0,
                    created_at: None,
                },
            ];

            // First import — both should be new
            let s1 = import_memories(&root, &entries, None).expect("import 1");
            assert_eq!(s1.imported, 2, "first import: 2 new rows");
            assert_eq!(s1.deduped, 0, "first import: no dups");

            let count_after_first = list_memories(&root).expect("list after 1st").len();
            assert_eq!(count_after_first, 2);

            // Second import — same entries, all collapsed by normalized-text dedup
            let s2 = import_memories(&root, &entries, None).expect("import 2");
            assert_eq!(s2.imported, 0, "second import: no new rows");
            assert_eq!(s2.deduped, 2, "second import: both entries deduped");

            let count_after_second = list_memories(&root).expect("list after 2nd").len();
            assert_eq!(
                count_after_second, 2,
                "list_memories count must be unchanged after second import"
            );

            fs::remove_dir_all(&root).ok();
        });
    }

    /// scope_override: importing with `Some(GlobalUser)` with user brain disabled
    /// routes entries to the project DB under global_user scope.
    #[test]
    fn import_scope_override_global_user() {
        with_user_brain_disabled(|| {
            // With user brain disabled, GlobalUser writes fall through to project DB.
            let root = test_root();
            init_project(&root, false).expect("init");

            let entries = vec![MemoryExport {
                text: "scope override test memory".to_string(),
                scope: "project".to_string(), // original scope — will be overridden
                kind: "fact".to_string(),
                confidence: 1.0,
                created_at: None,
            }];

            let summary =
                import_memories(&root, &entries, Some(MemoryScope::GlobalUser)).expect("import");
            assert_eq!(summary.imported, 1);
            assert_eq!(summary.deduped, 0);

            // The memory must appear with scope = global_user in the project DB
            // (since user brain is disabled, GlobalUser falls through to project).
            let mems = list_memories(&root).expect("list");
            assert_eq!(mems.len(), 1);
            assert_eq!(
                mems[0].scope, "global_user",
                "scope_override must win over entry.scope"
            );
            assert_eq!(mems[0].text, "scope override test memory");

            fs::remove_dir_all(&root).ok();
        });
    }

    /// Malformed entries (bad scope or kind string) are skipped gracefully;
    /// valid entries in the same batch are still imported.
    #[test]
    fn import_skips_malformed_entries() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let entries = vec![
                // valid
                MemoryExport {
                    text: "good entry".to_string(),
                    scope: "project".to_string(),
                    kind: "fact".to_string(),
                    confidence: 1.0,
                    created_at: None,
                },
                // bad scope
                MemoryExport {
                    text: "bad scope entry".to_string(),
                    scope: "not_a_real_scope".to_string(),
                    kind: "fact".to_string(),
                    confidence: 1.0,
                    created_at: None,
                },
                // bad kind
                MemoryExport {
                    text: "bad kind entry".to_string(),
                    scope: "project".to_string(),
                    kind: "not_a_real_kind".to_string(),
                    confidence: 1.0,
                    created_at: None,
                },
                // another valid
                MemoryExport {
                    text: "second good entry".to_string(),
                    scope: "repo".to_string(),
                    kind: "convention".to_string(),
                    confidence: 1.0,
                    created_at: None,
                },
            ];

            let summary = import_memories(&root, &entries, None).expect("import with bad entries");
            assert_eq!(
                summary.imported, 2,
                "2 valid entries must be imported; got {summary:?}"
            );
            assert_eq!(
                summary.deduped, 2,
                "2 malformed entries counted as skipped/deduped; got {summary:?}"
            );

            let mems = list_memories(&root).expect("list");
            assert_eq!(mems.len(), 2, "exactly 2 memories in DB; got {mems:?}");
            let texts: Vec<&str> = mems.iter().map(|m| m.text.as_str()).collect();
            assert!(
                texts.contains(&"good entry"),
                "good entry missing: {texts:?}"
            );
            assert!(
                texts.contains(&"second good entry"),
                "second good entry missing: {texts:?}"
            );

            fs::remove_dir_all(&root).ok();
        });
    }

    // ── Q5b: export redact ────────────────────────────────────────────────────

    /// Pure-fn tests for `redact_context_suffix` edge cases.
    #[test]
    fn redact_context_suffix_strips_trailing_context() {
        assert_eq!(
            redact_context_suffix("always use --locked (context: cargo build)"),
            "always use --locked"
        );
        // Multiple spaces before (context: …) are consumed by trim_end.
        assert_eq!(
            redact_context_suffix("lesson body   (context: some task)"),
            "lesson body"
        );
        // No pattern → unchanged.
        assert_eq!(redact_context_suffix("bare lesson"), "bare lesson");
        // Safety fallback: stripping would leave empty → original returned.
        assert_eq!(
            redact_context_suffix("(context: only context)"),
            "(context: only context)"
        );
        // Nested parens in context segment — only the outermost suffix is stripped.
        assert_eq!(
            redact_context_suffix("lesson (context: (nested) task)"),
            "lesson"
        );
        // Trailing whitespace after the close paren is tolerated by trim_end.
        assert_eq!(redact_context_suffix("lesson (context: task)  "), "lesson");
    }

    /// Pure-fn tests for `redact_tags_prefix` edge cases.
    #[test]
    fn redact_tags_prefix_strips_leading_tags() {
        assert_eq!(
            redact_tags_prefix("[tags: rust, cargo] always use --locked"),
            "always use --locked"
        );
        // No pattern → unchanged.
        assert_eq!(redact_tags_prefix("no tags here"), "no tags here");
        // Safety fallback: stripping would leave empty → original returned.
        assert_eq!(redact_tags_prefix("[tags: only-tag]"), "[tags: only-tag]");
        // Leading whitespace before [tags: is preserved by trim_start then not stripped.
        assert_eq!(redact_tags_prefix("  [tags: rust] lesson"), "lesson");
    }

    /// `apply_export_redaction` with both flags false → no change.
    #[test]
    fn apply_export_redaction_no_flags_is_passthrough() {
        let entry = MemoryExport {
            text: "[tags: rust] lesson (context: task)".to_string(),
            scope: "project".to_string(),
            kind: "fact".to_string(),
            confidence: 1.0,
            created_at: None,
        };
        let out = apply_export_redaction(entry.clone(), false, false);
        assert_eq!(out.text, entry.text);
    }

    /// `apply_export_redaction` with `redact=true` strips context only.
    #[test]
    fn apply_export_redaction_redact_only_strips_context() {
        let entry = MemoryExport {
            text: "[tags: rust] lesson (context: task)".to_string(),
            scope: "project".to_string(),
            kind: "fact".to_string(),
            confidence: 1.0,
            created_at: None,
        };
        let out = apply_export_redaction(entry, true, false);
        assert_eq!(out.text, "[tags: rust] lesson");
    }

    /// `apply_export_redaction` with both flags strips tags then context.
    #[test]
    fn apply_export_redaction_both_flags_strips_tags_and_context() {
        let entry = MemoryExport {
            text: "[tags: rust, cargo] lesson (context: task)".to_string(),
            scope: "project".to_string(),
            kind: "fact".to_string(),
            confidence: 1.0,
            created_at: None,
        };
        let out = apply_export_redaction(entry, true, true);
        assert_eq!(out.text, "lesson");
    }

    /// End-to-end: export with `--redact`, import, then re-import deduplicates.
    ///
    /// Verifies that the normalized-text dedup path works correctly with
    /// redacted texts — the stripped form must normalize identically on
    /// second import.
    #[test]
    fn export_redact_import_roundtrip_and_dedup() {
        with_user_brain_disabled(|| {
            let root_a = test_root();
            init_project(&root_a, false).expect("init A");

            // Seed a memory that has the context suffix the distiller adds.
            add_memory(
                &root_a,
                MemoryScope::Project,
                MemoryKind::Fact,
                "use --locked for reproducibility (context: cargo test failing)",
            )
            .expect("add memory");

            // Export with redact=true.
            let (exported, _) =
                export_memories(&root_a, None, None, true, false).expect("export redacted");
            assert_eq!(exported.len(), 1);
            assert_eq!(
                exported[0].text, "use --locked for reproducibility",
                "context suffix must be stripped"
            );

            // Import into a fresh project.
            let root_b = test_root();
            init_project(&root_b, false).expect("init B");
            let s1 = import_memories(&root_b, &exported, None).expect("import 1");
            assert_eq!(s1.imported, 1, "first import must create 1 row");
            assert_eq!(s1.deduped, 0);

            // Re-import the same redacted slice → must dedup, not double-insert.
            let s2 = import_memories(&root_b, &exported, None).expect("import 2");
            assert_eq!(s2.imported, 0, "second import must dedup");
            assert_eq!(s2.deduped, 1);

            // List shows the redacted text (not the original context-annotated form).
            let mems = list_memories(&root_b).expect("list");
            assert_eq!(mems.len(), 1);
            assert_eq!(mems[0].text, "use --locked for reproducibility");

            fs::remove_dir_all(&root_a).ok();
            fs::remove_dir_all(&root_b).ok();
        });
    }

    // ── v3.0 #4: shareable pack install (merge | replace + provenance) ──────
    #[test]
    fn import_pack_merge_replace_and_provenance() {
        with_user_brain_disabled(|| {
            let root_a = test_root();
            init_project(&root_a, false).expect("init A");
            add_memory(
                &root_a,
                MemoryScope::Project,
                MemoryKind::Convention,
                "use cargo --locked",
            )
            .expect("a1");
            add_memory(
                &root_a,
                MemoryScope::Project,
                MemoryKind::Fact,
                "brain db lives in dot kimetsu",
            )
            .expect("a2");

            // Export → wrap as a Pack envelope → parse back (round-trip).
            let (entries, scrub) =
                export_memories(&root_a, None, None, false, false).expect("export");
            assert!(scrub.is_clean(), "clean memories must scrub to nothing");
            let pack = Pack {
                kimetsu_pack: 1,
                name: Some("demo".into()),
                version: Some("1.0".into()),
                description: None,
                exported_at: None,
                memory_count: entries.len(),
                memories: entries.clone(),
            };
            let json = serde_json::to_string(&pack).expect("ser");
            let (pref, parsed) = parse_pack_or_array(&json).expect("parse");
            assert_eq!(pref.name.as_deref(), Some("demo"));
            assert_eq!(parsed.len(), 2);
            // Bare array also parses (back-compat).
            let (bare_ref, bare) =
                parse_pack_or_array(&serde_json::to_string(&entries).unwrap()).expect("parse bare");
            assert!(bare_ref.name.is_none());
            assert_eq!(bare.len(), 2);

            // Install (merge) into B, which already has its own memory.
            let root_b = test_root();
            init_project(&root_b, false).expect("init B");
            add_memory(
                &root_b,
                MemoryScope::Project,
                MemoryKind::Fact,
                "B's own memory",
            )
            .expect("b1");
            let s = import_pack(&root_b, &parsed, None, false, Some(&pref)).expect("merge");
            assert_eq!(s.imported, 2, "two new pack memories");
            assert_eq!(s.superseded, 0);

            // Pack memories carry provenance source=="pack".
            let pack_tagged = |root: &Path| -> i64 {
                let (_p, _c, conn) = load_project_readonly(root).expect("ro");
                conn.query_row(
                    "SELECT COUNT(*) FROM memories
                     WHERE provenance_snapshot_json LIKE '%\"source\":\"pack\"%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(
                pack_tagged(&root_b),
                2,
                "installed memories tagged with pack provenance"
            );

            // Re-install (merge) → all deduped.
            let s2 = import_pack(&root_b, &parsed, None, false, Some(&pref)).expect("merge2");
            assert_eq!(s2.imported, 0);
            assert_eq!(s2.deduped, 2);

            // Replace: B's current project memories (its own + the 2 pack) are
            // superseded, then the pack reloads → 2 active project memories.
            let s3 = import_pack(&root_b, &parsed, None, true, Some(&pref)).expect("replace");
            assert_eq!(
                s3.superseded, 3,
                "all 3 active project memories invalidated"
            );
            assert_eq!(s3.imported, 2, "pack reloaded as fresh rows");
            let active_project = {
                let (_p, _c, conn) = load_project_readonly(&root_b).expect("ro2");
                conn.query_row(
                    "SELECT COUNT(*) FROM memories
                     WHERE scope='project' AND invalidated_at IS NULL AND superseded_by IS NULL",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
            };
            assert_eq!(
                active_project, 2,
                "only the pack's 2 memories remain active"
            );

            fs::remove_dir_all(&root_a).ok();
            fs::remove_dir_all(&root_b).ok();
        });
    }

    // ── Q6: memory edit / memory undo ──────────────────────────────────────

    /// Q6-1: edit_memory updates text + normalized_text + FTS, preserves history.
    #[test]
    fn edit_memory_updates_text_and_preserves_history() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "original text for edit test",
            )
            .expect("add");

            // Simulate a "learned" memory by bumping use_count and usefulness_score.
            {
                let (_p, _c, conn) = load_project(&root).expect("open conn");
                conn.execute(
                    "UPDATE memories SET use_count = 7, usefulness_score = 3.5 WHERE memory_id = ?1",
                    params![mid],
                )
                .expect("bump counters");
            }

            // Edit the text in place.
            edit_memory(&root, &mid, Some("corrected text for edit test"), None)
                .expect("edit_memory");

            // Verify text + normalized_text changed.
            {
                let (_p, _c, conn) = load_project(&root).expect("open conn");
                let (text, normalized, use_count, usefulness_score): (String, String, i64, f64) =
                    conn.query_row(
                        "SELECT text, normalized_text, use_count, usefulness_score FROM memories WHERE memory_id = ?1",
                        params![mid],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .expect("query");

                assert_eq!(text, "corrected text for edit test");
                assert!(!normalized.is_empty(), "normalized_text must not be empty");
                // History preserved.
                assert_eq!(use_count, 7, "use_count must not be reset");
                assert!(
                    (usefulness_score - 3.5).abs() < 0.01,
                    "usefulness_score must not be reset"
                );
            }

            // FTS reflects new text — search for a word in the new text.
            let hits = search_memories(&root, "corrected", 10, 0, None, None).expect("search new");
            assert!(
                hits.iter().any(|h| h.memory_id == mid),
                "edited text must appear in FTS search: {hits:?}"
            );

            // Old text must no longer match.
            let old_hits =
                search_memories(&root, "original", 10, 0, None, None).expect("search old");
            assert!(
                !old_hits.iter().any(|h| h.memory_id == mid),
                "old text must NOT appear after edit: {old_hits:?}"
            );

            // list_memories should return the new text.
            let mems = list_memories(&root).expect("list");
            let m = mems.iter().find(|m| m.memory_id == mid).expect("found");
            assert_eq!(m.text, "corrected text for edit test");
        });
    }

    /// Q6-2: edit_memory can change kind without touching text.
    #[test]
    fn edit_memory_changes_kind_only() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "kind-change test memory",
            )
            .expect("add");

            edit_memory(&root, &mid, None, Some(MemoryKind::Convention)).expect("edit kind");

            let mems = list_memories(&root).expect("list");
            let m = mems.iter().find(|m| m.memory_id == mid).expect("found");
            assert_eq!(m.kind, "convention", "kind must be updated");
            assert_eq!(m.text, "kind-change test memory", "text must be unchanged");
        });
    }

    /// Q6-3: edit_memory errors on unknown id, invalidated id, and neither arg.
    #[test]
    fn edit_memory_errors() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Neither text nor kind → error.
            let err = edit_memory(&root, "does-not-matter", None, None)
                .expect_err("must err when no fields");
            assert!(
                format!("{err}").contains("at least one"),
                "unexpected err: {err}"
            );

            // Unknown id.
            let err = edit_memory(&root, "UNKNOWN_ID", Some("x"), None)
                .expect_err("must err on unknown id");
            assert!(
                format!("{err}").contains("not found"),
                "unexpected err: {err}"
            );

            // Invalidated id.
            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "will be invalidated",
            )
            .expect("add");
            invalidate_memory(&root, &mid, None).expect("invalidate");
            let err = edit_memory(&root, &mid, Some("new text"), None)
                .expect_err("must err on invalidated id");
            assert!(
                format!("{err}").contains("invalidated"),
                "unexpected err: {err}"
            );
        });
    }

    /// Q6-4: undo_last_memory invalidates the most recent memory; second call
    /// invalidates the one before it.
    #[test]
    fn undo_last_memory_invalidates_newest_first() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let mid_a = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "memory A older undo test",
            )
            .expect("add A");

            let mid_b = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "memory B newer undo test",
            )
            .expect("add B");

            // First undo → B (the newer one per created_at DESC, memory_id DESC).
            let undone = undo_last_memory(&root)
                .expect("undo 1")
                .expect("must return Some");
            assert_eq!(undone.memory_id, mid_b, "undo must target B (newest)");

            // Check B is now invalidated via DB query.
            {
                let (_p, _c, conn) = load_project(&root).expect("open conn");
                let b_inv: Option<String> = conn
                    .query_row(
                        "SELECT invalidated_at FROM memories WHERE memory_id = ?1",
                        params![mid_b],
                        |row| row.get(0),
                    )
                    .optional()
                    .expect("query")
                    .flatten();
                assert!(b_inv.is_some(), "B must be invalidated after undo");

                let a_inv: Option<String> = conn
                    .query_row(
                        "SELECT invalidated_at FROM memories WHERE memory_id = ?1",
                        params![mid_a],
                        |row| row.get(0),
                    )
                    .optional()
                    .expect("query")
                    .flatten();
                assert!(a_inv.is_none(), "A must still be active");
            }

            // Second undo → A.
            let undone2 = undo_last_memory(&root)
                .expect("undo 2")
                .expect("must return Some");
            assert_eq!(undone2.memory_id, mid_a, "second undo must target A");

            // Both invalidated.
            {
                let (_p, _c, conn) = load_project(&root).expect("open conn");
                let a_inv: Option<String> = conn
                    .query_row(
                        "SELECT invalidated_at FROM memories WHERE memory_id = ?1",
                        params![mid_a],
                        |row| row.get(0),
                    )
                    .optional()
                    .expect("query")
                    .flatten();
                assert!(a_inv.is_some(), "A must be invalidated after second undo");
            }

            // peek_last_memory returns None after both are invalidated.
            let peek = peek_last_memory(&root).expect("peek after both undone");
            assert!(peek.is_none(), "peek must return None when all invalidated");
        });
    }

    /// Q6-5: undo_last_memory on an empty brain returns Ok(None).
    #[test]
    fn undo_last_memory_on_empty_brain_returns_none() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let result = undo_last_memory(&root).expect("undo on empty");
            assert!(result.is_none(), "must return None on empty brain");
        });
    }

    // ── Q8: compact_brain tests ───────────────────────────────────────────────

    /// Q8-1: VACUUM reclaims space after purging invalidated memories.
    ///
    /// Adds enough memories to grow the file, invalidates most of them,
    /// then calls compact_brain with purge_invalidated=true. After compaction:
    ///   - bytes_after <= bytes_before (VACUUM at minimum doesn't grow the file)
    ///   - invalidated_memories_purged > 0
    ///   - active memories still survive and are retrievable
    #[test]
    fn compact_brain_purge_invalidated_reclaims_space() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Add 20 memories — enough to make the file non-trivially sized.
            let mut active_id = String::new();
            for i in 0..20usize {
                let text = format!(
                    "compact test memory number {i}: rust sqlite vacuum reclaim disk space \
                     kimetsu brain compact test payload to increase file size substantially \
                     so that vacuum has meaningful dead pages to reclaim after deletion"
                );
                let mid = add_memory(&root, MemoryScope::Project, MemoryKind::Fact, &text)
                    .expect("add memory");
                if i == 0 {
                    active_id = mid.clone();
                }
                // Invalidate all but the first one.
                if i > 0 {
                    invalidate_memory(&root, &mid, Some("compact test"))
                        .expect("invalidate memory");
                }
            }

            // Run compact with purge_invalidated = true.
            let report = compact_brain(&root, None, true).expect("compact_brain");

            // Purge count must match the 19 invalidated memories.
            assert_eq!(
                report.invalidated_memories_purged, 19,
                "should have purged 19 invalidated memories, got {}",
                report.invalidated_memories_purged
            );
            // bytes_after must not exceed bytes_before (VACUUM can only shrink or equal).
            assert!(
                report.bytes_after <= report.bytes_before,
                "bytes_after ({}) should be <= bytes_before ({}) after purge+vacuum",
                report.bytes_after,
                report.bytes_before
            );
            // events_trimmed must be 0 (we didn't request a trim).
            assert_eq!(
                report.events_trimmed, 0,
                "events_trimmed must be 0 when trim_events_older_than is None"
            );

            // The one active memory must still be listable.
            let memories = list_memories(&root).expect("list memories after compact");
            let active_memories: Vec<_> = memories
                .iter()
                .filter(|m| m.memory_id == active_id)
                .collect();
            assert_eq!(
                active_memories.len(),
                1,
                "the active memory must survive compaction"
            );
        });
    }

    /// Q8-2: default compact (no flags) preserves everything — a pure VACUUM.
    ///
    /// All memories (active AND invalidated) survive, events are untouched,
    /// and both counters are 0.
    #[test]
    fn compact_brain_default_preserves_everything() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "preserve me through compact",
            )
            .expect("add memory");
            let mid2 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "preserve invalidated too",
            )
            .expect("add memory 2");
            invalidate_memory(&root, &mid2, Some("test")).expect("invalidate");

            // Count events before.
            let event_count_before: i64 = {
                let (_p, _c, conn) = load_project(&root).expect("load");
                conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                    .expect("count events")
            };

            // Default compact: no purge, no trim.
            let report = compact_brain(&root, None, false).expect("compact_brain");
            assert_eq!(
                report.events_trimmed, 0,
                "events_trimmed must be 0 in default compact"
            );
            assert_eq!(
                report.invalidated_memories_purged, 0,
                "invalidated_memories_purged must be 0 in default compact"
            );

            // All memories still present (active + invalidated).
            let all_mems: Vec<_> = {
                let (_p, _c, conn) = load_project(&root).expect("load");
                let mut stmt = conn
                    .prepare("SELECT memory_id FROM memories")
                    .expect("prepare");
                stmt.query_map([], |r| r.get::<_, String>(0))
                    .expect("query")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("collect")
            };
            assert!(
                all_mems.contains(&mid),
                "active memory must survive default compact"
            );
            assert!(
                all_mems.contains(&mid2),
                "invalidated memory must survive default compact"
            );

            // Event count unchanged.
            let event_count_after: i64 = {
                let (_p, _c, conn) = load_project(&root).expect("load");
                conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                    .expect("count events")
            };
            assert_eq!(
                event_count_after, event_count_before,
                "event count must not change in default compact"
            );
        });
    }

    /// Q8-3: event trim removes old events but materialized memories survive.
    ///
    /// Uses trim_events_older_than = Duration::ZERO so ALL events are
    /// classified as "old" relative to `now`. After trim:
    ///   - events_trimmed > 0
    ///   - list_memories still returns the seeded memory (projection survives)
    ///   - memories are NOT deleted by event trimming
    #[test]
    fn compact_brain_event_trim_keeps_materialized_memories() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "this memory must survive event trim",
            )
            .expect("add memory");

            // Trim with a 1-second Duration — but we add a 2-second sleep
            // alternative: use Duration::from_secs(0) which means cutoff =
            // now, so events older than "right now" are ALL deleted.
            // Using 0 ensures even events written 1ms ago are trimmed.
            let trim_dur = std::time::Duration::from_secs(0);

            // Small sleep to ensure events are definitively in the past
            // relative to the cutoff computed inside compact_brain.
            std::thread::sleep(std::time::Duration::from_millis(100));

            let report = compact_brain(&root, Some(trim_dur), false).expect("compact_brain");

            assert!(
                report.events_trimmed > 0,
                "events_trimmed should be > 0 after trim with duration=0; got {}",
                report.events_trimmed
            );

            // The materialized memory (projection row) must survive.
            let memories = list_memories(&root).expect("list memories after event trim");
            let found = memories.iter().any(|m| m.memory_id == mid);
            assert!(
                found,
                "memory must still be in the projection after event trim"
            );

            // purge count must be 0 — we didn't ask for it.
            assert_eq!(
                report.invalidated_memories_purged, 0,
                "invalidated_memories_purged must be 0 when purge_invalidated=false"
            );
        });
    }

    /// Q8-4: rebuild_projection after event trim does not error.
    ///
    /// Even with a partially trimmed event log, rebuild_in_place can complete —
    /// it replays whatever events remain without panicking or returning an error.
    #[test]
    fn compact_brain_event_trim_then_rebuild_is_consistent() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "pre-trim memory for rebuild test",
            )
            .expect("add memory");

            // Trim all events (cutoff = now).
            std::thread::sleep(std::time::Duration::from_millis(100));
            let report = compact_brain(&root, Some(std::time::Duration::from_secs(0)), false)
                .expect("compact_brain");
            assert!(report.events_trimmed > 0, "events must have been trimmed");

            // rebuild_projection must not error — it replays whatever events remain.
            let replayed =
                rebuild_projection(&root, false).expect("rebuild_projection after event trim");
            // The events are gone so the replay count should be 0 (empty log).
            assert_eq!(
                replayed, 0,
                "replayed should be 0 after all events are trimmed"
            );
        });
    }

    // ── *_at_root no-git seam tests ───────────────────────────────────────

    /// init_project_at_root creates .kimetsu/{project.toml,brain.db} rooted
    /// at the given directory even when that directory lives INSIDE a git repo
    /// (no git climb). load_project_at_root opens it, and a round-trip memory
    /// add + list confirms the brain is functional.
    #[test]
    fn at_root_init_and_round_trip_memory() {
        with_user_brain_disabled(|| {
            // Use a temp dir with a git boundary so that `add_memory` (which
            // uses ProjectPaths::discover internally) resolves to this dir
            // rather than climbing to E:\Kimetsu. The *_at_root functions
            // themselves never call discover; the boundary is only needed for
            // the helper calls (add_memory / list_memories) in this test.
            let root = std::env::temp_dir().join(format!("kimetsu-at-root-{}", Ulid::new()));
            kimetsu_core::paths::git_init_boundary(&root);

            // Init at explicit root — must not climb to a parent git repo.
            let summary = init_project_at_root(&root, false).expect("init_project_at_root");

            assert!(
                summary.kimetsu_dir.exists(),
                ".kimetsu/ must be created at root"
            );
            // The .kimetsu dir must be a child of root, not some git ancestor.
            assert!(
                summary.kimetsu_dir.starts_with(&root),
                ".kimetsu dir {:?} must be inside root {:?}",
                summary.kimetsu_dir,
                root
            );
            assert!(summary.brain_db.exists(), "brain.db must exist");
            assert!(
                root.join(".kimetsu").join("project.toml").exists(),
                "project.toml must be at root/.kimetsu/"
            );

            // load_project_at_root must open the same brain.
            let (paths, _config, _conn) =
                load_project_at_root(&root).expect("load_project_at_root");
            assert_eq!(
                paths
                    .repo_root
                    .canonicalize()
                    .unwrap_or(paths.repo_root.clone()),
                root.canonicalize().unwrap_or(root.clone()),
                "repo_root must be our explicit root"
            );

            // Round-trip: add a memory, then verify it is visible via list_memories.
            let memory_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "at_root seam test memory",
            )
            .expect("add_memory");

            // list_memories opens a fresh connection — confirms the write landed
            // in the at-root brain.db (not a git-ancestor brain).
            let memories = list_memories(&root).expect("list_memories");
            assert!(
                memories.iter().any(|m| m.memory_id == memory_id),
                "memory {memory_id} must be present in the at_root brain"
            );

            // load_project_readonly_at_root must also see it.
            let (_, _, ro_conn) =
                load_project_readonly_at_root(&root).expect("load_project_readonly_at_root");
            let ro_count: i64 = ro_conn
                .query_row(
                    "SELECT COUNT(*) FROM memories WHERE memory_id = ?1",
                    rusqlite::params![memory_id],
                    |row| row.get(0),
                )
                .expect("count memory ro");
            assert_eq!(ro_count, 1, "readonly view must see the same memory");

            std::fs::remove_dir_all(&root).ok();
        });
    }

    /// init_project_at_root is idempotent: calling it twice (force=false)
    /// does not overwrite project.toml.
    #[test]
    fn at_root_init_is_idempotent() {
        with_user_brain_disabled(|| {
            let root = std::env::temp_dir().join(format!("kimetsu-at-root-idem-{}", Ulid::new()));
            std::fs::create_dir_all(&root).expect("create root");

            let s1 = init_project_at_root(&root, false).expect("first init");
            assert!(s1.wrote_project_toml, "first init must write project.toml");

            let s2 = init_project_at_root(&root, false).expect("second init");
            assert!(
                !s2.wrote_project_toml,
                "second init (force=false) must not overwrite project.toml"
            );
            assert_eq!(s1.project_id, s2.project_id, "project_id must be stable");

            std::fs::remove_dir_all(&root).ok();
        });
    }

    // ------------------------------------------------------------------
    // Fix 2: detect_conflicts off-switch (end-to-end via add_memory)
    // ------------------------------------------------------------------

    /// Fix 2: with KIMETSU_DETECT_CONFLICTS=0 in the env, add_memory of a
    /// near-duplicate writes no row to memory_conflicts even when the brain
    /// has an active near-dup. Verifies the env > config precedence.
    #[test]
    fn detect_conflicts_env_off_writes_no_conflict_rows() {
        // with_user_brain_disabled already holds test_env_lock — do NOT
        // lock again (non-reentrant mutex → deadlock).
        with_user_brain_disabled(|| {
            let prev_dc = std::env::var("KIMETSU_DETECT_CONFLICTS").ok();
            let prev_emb = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();

            // Disable embedder (noop) so the test stays fast and
            // deterministic — conflict detection is a no-op on Noop anyway,
            // but the off-switch is also applied on non-noop builds.
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
                std::env::remove_var("KIMETSU_DETECT_CONFLICTS");
            }

            let root = test_root();
            init_project(&root, false).expect("init");

            // With detection enabled (default) and noop embedder:
            // no conflicts will fire regardless (noop short-circuits).
            // The real test is the config-level gate, tested in conflict.rs.
            // Here we exercise the project path end-to-end.
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "use clippy for linting Rust code",
            )
            .expect("add 1");

            // Now disable via env.
            unsafe {
                std::env::set_var("KIMETSU_DETECT_CONFLICTS", "0");
            }
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "use clippy for linting all Rust projects",
            )
            .expect("add 2");

            // Restore env.
            unsafe {
                match prev_dc {
                    Some(v) => std::env::set_var("KIMETSU_DETECT_CONFLICTS", v),
                    None => std::env::remove_var("KIMETSU_DETECT_CONFLICTS"),
                }
                match prev_emb {
                    Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                    None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
                }
            }
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // ------------------------------------------------------------------
    // Micro-benchmark: Fix 4 — per-add cost must not scale linearly with N
    // ------------------------------------------------------------------

    /// Structural invariant: after seeding N memories, the active-memory count
    /// matches the number of adds.
    ///
    /// The micro-benchmark times an early vs late add (with conflict detection
    /// OFF to isolate per-add maintenance cost) and asserts the late add is not
    /// dramatically slower — proving O(1) per-add cost (the usearch index is
    /// maintained incrementally, never full-scanned on add).
    #[test]
    fn perf_tier1_structural_invariant_and_timing() {
        // with_user_brain_disabled already holds test_env_lock — do NOT
        // lock again (non-reentrant mutex → deadlock).
        with_user_brain_disabled(|| {
            #[allow(unused_imports)]
            use crate::embeddings::StubEmbedder;
            use std::time::Instant;

            let prev_dc = std::env::var("KIMETSU_DETECT_CONFLICTS").ok();
            let prev_emb = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();

            // Disable conflict detection so we isolate vec-table cost.
            // Use "noop" embedder to keep the test fast.
            unsafe {
                std::env::set_var("KIMETSU_DETECT_CONFLICTS", "0");
                std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop"); // keep fast
            }

            let root = test_root();
            init_project(&root, false).expect("init");

            const EARLY_SAMPLE: usize = 100;
            const TOTAL: usize = 200; // keep test fast

            // Warm up and measure early add (after ~100 rows).
            for i in 0..EARLY_SAMPLE {
                add_memory(
                    &root,
                    MemoryScope::Project,
                    MemoryKind::Fact,
                    &format!("perf test memory row {i} unique content abcdef"),
                )
                .expect("add early");
            }

            let t_early = Instant::now();
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                &format!("perf sampled early add memory row {EARLY_SAMPLE} unique zxcvbn"),
            )
            .expect("timed early add");
            let early_us = t_early.elapsed().as_micros();

            // Fill up to TOTAL.
            for i in (EARLY_SAMPLE + 1)..TOTAL {
                add_memory(
                    &root,
                    MemoryScope::Project,
                    MemoryKind::Fact,
                    &format!("perf test memory row {i} unique content qwerty"),
                )
                .expect("add fill");
            }

            let t_late = Instant::now();
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                &format!("perf sampled late add memory row {TOTAL} unique rtyfgh"),
            )
            .expect("timed late add");
            let late_us = t_late.elapsed().as_micros();

            // Structural invariant: memories count matches (roughly) total adds.
            let (_, _, conn) = load_project(&root).expect("load");
            let mem_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .expect("count memories");
            // We added TOTAL + 2 timed samples = TOTAL + 2.
            assert!(
                mem_count >= TOTAL as i64,
                "must have at least {TOTAL} memories, got {mem_count}"
            );

            // Timing invariant: late add must not be > 20× slower than early add
            // (generous bound; O(1) should be near-equal, O(N) would be ≫).
            // Only assert when both samples are > 0 to avoid flakes on fast CI.
            if early_us > 0 && late_us > 0 {
                assert!(
                    late_us < early_us * 20,
                    "late add ({late_us}µs) is > 20× slower than early add ({early_us}µs) — O(N) regression"
                );
            }

            // Restore env.
            unsafe {
                match prev_dc {
                    Some(v) => std::env::set_var("KIMETSU_DETECT_CONFLICTS", v),
                    None => std::env::remove_var("KIMETSU_DETECT_CONFLICTS"),
                }
                match prev_emb {
                    Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                    None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
                }
            }
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[cfg(feature = "embeddings")]
    #[test]
    fn ann_retrieval_round_trips_and_invalidate_drops() {
        use crate::user_brain::with_user_brain_disabled;
        with_user_brain_disabled(|| {
            // Use the StubEmbedder so this is deterministic and offline.
            let prev_emb = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "stub-d8");
            }
            let root = test_root();
            init_project(&root, false).expect("init");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "ripgrep is the fast recursive search tool",
            )
            .expect("add a");
            let id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "use fd to find files quickly",
            )
            .expect("add b");

            // Retrieval surfaces the relevant memory via the ANN path.
            let ctx = retrieve_context(&root, "recall", "find files fast", 1024).expect("ctx");
            assert!(
                format!("{ctx:?}").contains("fd to find files"),
                "expected the fd memory in context"
            );

            // Invalidate it -> it disappears from retrieval.
            invalidate_memory(&root, &id, Some("test")).expect("invalidate");
            let ctx2 = retrieve_context(&root, "recall", "find files fast", 1024).expect("ctx2");
            assert!(
                !format!("{ctx2:?}").contains("fd to find files"),
                "invalidated memory must not return"
            );

            unsafe {
                match prev_emb {
                    Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                    None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
                }
            }
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // v1.5: record_mcp_citation
    #[test]
    fn record_mcp_citation_writes_memory_citations_row() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "record_mcp_citation test fixture",
            )
            .expect("add memory");

            record_mcp_citation(&root, &memory_id, Some("helped with test"))
                .expect("record_mcp_citation");

            let (_paths, _config, conn) = load_project(&root).expect("load");
            let row_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_citations WHERE memory_id = ?1",
                    rusqlite::params![&memory_id],
                    |r| r.get(0),
                )
                .expect("count");
            assert_eq!(
                row_count, 1,
                "memory_citations row must exist after MCP cite"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // Phase 2 keyless: record_regret injects a retrieval.regret event.
    #[test]
    fn record_regret_writes_retrieval_regret_event() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "record_regret test fixture",
            )
            .expect("add memory");

            record_regret(&root, &memory_id).expect("record_regret");

            let (_paths, _config, conn) = load_project(&root).expect("load");
            let event_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events
                     WHERE kind = 'retrieval.regret'
                       AND json_extract(payload_json, '$.memory_id') = ?1",
                    rusqlite::params![&memory_id],
                    |r| r.get(0),
                )
                .expect("count");
            assert_eq!(
                event_count, 1,
                "a retrieval.regret event must exist for the memory after record_regret"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // Story 2.4: read (use_count, usefulness_score, confidence) for a memory.
    #[cfg(test)]
    fn read_outcome_stats(root: &std::path::Path, memory_id: &str) -> (i64, f64, f64) {
        let (_paths, _config, conn) = load_project(root).expect("load");
        conn.query_row(
            "SELECT use_count, usefulness_score, confidence FROM memories WHERE memory_id = ?1",
            rusqlite::params![memory_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("stats")
    }

    // Story 2.4: a standalone citation raises use_count + usefulness (outcome
    // signal applied because the run_id is the sentinel).
    #[test]
    fn standalone_cite_raises_usefulness() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "standalone cite outcome fixture",
            )
            .expect("add memory");

            let (uc0, us0, cf0) = read_outcome_stats(&root, &memory_id);
            record_mcp_citation(&root, &memory_id, None).expect("cite");
            let (uc1, us1, cf1) = read_outcome_stats(&root, &memory_id);

            assert_eq!(uc1, uc0 + 1, "use_count must increment on standalone cite");
            assert!(us1 > us0, "usefulness must rise: {us0} -> {us1}");
            // A fresh memory starts below the ceiling (DIRECT_ADD_CONFIDENCE), so a
            // positive outcome nudges confidence UP toward 1.0 — letting a proven
            // memory outrank a never-evaluated one.
            assert!(
                cf1 > cf0,
                "confidence must rise toward 1.0 on a positive outcome: {cf0} -> {cf1}"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // Story 2.4: a manual regret lowers usefulness AND confidence.
    #[test]
    fn manual_regret_lowers_usefulness_and_confidence() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "manual regret outcome fixture",
            )
            .expect("add memory");

            let (_uc0, us0, cf0) = read_outcome_stats(&root, &memory_id);
            record_regret(&root, &memory_id).expect("regret");
            let (_uc1, us1, cf1) = read_outcome_stats(&root, &memory_id);

            assert!(us1 < us0, "usefulness must drop on regret: {us0} -> {us1}");
            assert!(cf1 < cf0, "confidence must drop on regret: {cf0} -> {cf1}");
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // Story 2.4 safety: a citation tied to a REAL run does NOT bump stats in
    // apply_memory_cited (the run-finalization path owns that) — no double-count.
    #[test]
    fn real_run_cite_does_not_bump_in_apply_memory_cited() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "real run cite fixture",
            )
            .expect("add memory");

            let (uc0, us0, _cf0) = read_outcome_stats(&root, &memory_id);

            // A memory.cited event with a NON-nil (real) run_id.
            let real_run = kimetsu_core::ids::RunId::new();
            let event = kimetsu_core::event::Event::new(
                real_run,
                "memory.cited",
                serde_json::json!({ "memory_id": memory_id, "turn": 0 }),
            );
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                crate::projector::apply_events(&conn, std::slice::from_ref(&event)).expect("apply");
            }

            let (uc1, us1, _cf1) = read_outcome_stats(&root, &memory_id);
            assert_eq!(uc1, uc0, "real-run cite must NOT increment use_count here");
            assert!(
                (us1 - us0).abs() < 1e-9,
                "real-run cite must NOT change usefulness here"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // Story 2.4: outcome stats are event-sourced — a full rebuild replays the
    // cite/regret events and reproduces the same use_count/usefulness/confidence.
    #[test]
    fn cite_outcome_survives_rebuild() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "rebuild outcome fixture",
            )
            .expect("add memory");
            record_mcp_citation(&root, &memory_id, None).expect("cite");

            let before = read_outcome_stats(&root, &memory_id);
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                crate::projector::rebuild_in_place(&conn).expect("rebuild");
            }
            let after = read_outcome_stats(&root, &memory_id);
            assert_eq!(before.0, after.0, "use_count must survive rebuild");
            assert!(
                (before.1 - after.1).abs() < 1e-9,
                "usefulness must survive rebuild"
            );
            assert!(
                (before.2 - after.2).abs() < 1e-9,
                "confidence must survive rebuild"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // Age injection: set-age backdates created_at (and survives rebuild).
    #[test]
    fn set_age_backdates_created_at_and_survives_rebuild() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create root");
            init_project(&root, false).expect("init");
            let memory_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "age injection fixture",
            )
            .expect("add memory");

            let read_created = |root: &std::path::Path| -> String {
                let (_paths, _config, conn) = load_project(root).expect("load");
                conn.query_row(
                    "SELECT created_at FROM memories WHERE memory_id = ?1",
                    rusqlite::params![&memory_id],
                    |r| r.get::<_, String>(0),
                )
                .expect("created_at")
            };

            let created0 = read_created(&root);
            record_set_age(&root, &memory_id, 90).expect("set-age");
            let created1 = read_created(&root);
            // RFC3339 strings sort chronologically; 90 days ago < now.
            assert!(
                created1 < created0,
                "created_at must move into the past: {created0} -> {created1}"
            );

            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                crate::projector::rebuild_in_place(&conn).expect("rebuild");
            }
            assert_eq!(
                read_created(&root),
                created1,
                "aged created_at survives rebuild"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    // ------------------------------------------------------------------
    // Fix 2: search_memories must not return superseded rows
    // ------------------------------------------------------------------
    #[test]
    fn fix2_search_excludes_superseded_rows() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Add a memory that will be superseded, and a live one.
            let superseded_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "unique superseded keyword alpha",
            )
            .expect("add superseded");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "live memory unrelated topic",
            )
            .expect("add live");

            // Mark the first memory as superseded via direct SQL (simulating
            // a prior consolidation run).
            {
                let (_paths, _config, conn) = load_project(&root).expect("load for stamp");
                conn.execute(
                    "UPDATE memories SET superseded_by = 'fake-survivor' \
                     WHERE memory_id = ?1",
                    rusqlite::params![&superseded_id],
                )
                .expect("stamp superseded_by");
            }

            // Search must not return the superseded row.
            let hits = search_memories(&root, "unique superseded keyword alpha", 20, 0, None, None)
                .expect("search");
            assert!(
                !hits.iter().any(|h| h.memory_id == superseded_id),
                "superseded row must not appear in search results"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    // ------------------------------------------------------------------
    // Fix 4: list_memories_top and prune_low_usefulness must not include
    // superseded rows
    // ------------------------------------------------------------------
    #[test]
    fn fix4_top_excludes_superseded_rows() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Add a memory and give it a high score + use_count.
            let superseded_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "memory to be superseded with high usefulness",
            )
            .expect("add");

            // Stamp it as superseded AND give it high stats.
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute(
                    "UPDATE memories \
                     SET superseded_by = 'fake-survivor', \
                         use_count = 10, usefulness_score = 50.0 \
                     WHERE memory_id = ?1",
                    rusqlite::params![&superseded_id],
                )
                .expect("stamp");
            }

            // Also add a live memory with lower but real stats.
            let live_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "live memory with normal stats",
            )
            .expect("add live");
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute(
                    "UPDATE memories SET use_count = 5, usefulness_score = 5.0 \
                     WHERE memory_id = ?1",
                    rusqlite::params![&live_id],
                )
                .expect("seed stats");
            }

            let opts = TopOptions {
                scope: None,
                min_uses: 1,
                limit: 20,
            };
            let top = list_memories_top(&root, opts).expect("list_memories_top");

            assert!(
                !top.iter().any(|r| r.memory_id == superseded_id),
                "superseded row must not appear in top"
            );
            assert!(
                top.iter().any(|r| r.memory_id == live_id),
                "live row must appear in top"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn fix4_prune_excludes_superseded_rows() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let superseded_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "memory to be superseded with low usefulness",
            )
            .expect("add");

            // Stamp it as superseded AND give it a very negative score.
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute(
                    "UPDATE memories \
                     SET superseded_by = 'fake-survivor', \
                         use_count = 10, usefulness_score = -99.0 \
                     WHERE memory_id = ?1",
                    rusqlite::params![&superseded_id],
                )
                .expect("stamp");
            }

            // A live memory with a negative score (qualifies for prune).
            let live_id = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "live memory with negative usefulness for prune",
            )
            .expect("add live");
            {
                let (_paths, _config, conn) = load_project(&root).expect("load");
                conn.execute(
                    "UPDATE memories SET use_count = 5, usefulness_score = -5.0 \
                     WHERE memory_id = ?1",
                    rusqlite::params![&live_id],
                )
                .expect("seed stats");
            }

            let opts = PruneOptions {
                scope: None,
                min_uses: 1,
                max_ratio: -0.1,
                apply: false,
            };
            let summary = prune_low_usefulness(&root, opts).expect("prune");

            assert!(
                !summary
                    .candidates
                    .iter()
                    .any(|c| c.memory_id == superseded_id),
                "superseded row must not appear in prune candidates"
            );
            assert!(
                summary.candidates.iter().any(|c| c.memory_id == live_id),
                "live negative-score row must appear in prune candidates"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    // ── add_memories_batch ────────────────────────────────────────────────────

    /// Core correctness: N memories added via add_memories_batch must be
    /// present, retrievable, and survive rebuild_in_place — byte-identical to
    /// memories written by individual add_memory calls.
    ///
    /// Embedding check: in the lean build the active embedder is NoopEmbedder
    /// (embedding IS NULL), exactly the same as for single-add. In the
    /// `--features embeddings` build a real model is loaded once and all
    /// entries get non-NULL embeddings. The test asserts consistency: every
    /// batch-added memory has the same embedding_model value as a single-added
    /// memory written in the same process.
    #[test]
    fn add_memories_batch_present_retrievable_rebuild_safe() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            // --- Build 5 distinct batch entries ----------------------------
            let entries: Vec<BatchMemoryEntry> = (1..=5)
                .map(|i| BatchMemoryEntry {
                    text: format!(
                        "batch memory entry number {i} unique text for semantic distance"
                    ),
                    scope: kimetsu_core::memory::MemoryScope::Project,
                    kind: kimetsu_core::memory::MemoryKind::Fact,
                    valid_from: None,
                    valid_to: None,
                })
                .collect();

            let ids = add_memories_batch(&root, entries).expect("add_memories_batch");

            // Correct count returned.
            assert_eq!(ids.len(), 5, "expected 5 ids back; got {:?}", ids);
            // All ids must be non-empty strings (valid ULIDs).
            for id in &ids {
                assert!(!id.is_empty(), "id must not be empty");
            }

            // --- All memories visible in list --------------------------------
            let memories = list_memories(&root).expect("list_memories after batch");
            assert_eq!(
                memories.len(),
                5,
                "list_memories should return 5; got {:?}",
                memories.iter().map(|m| &m.memory_id).collect::<Vec<_>>()
            );
            let stored_ids: Vec<_> = memories.iter().map(|m| m.memory_id.clone()).collect();
            for id in &ids {
                assert!(stored_ids.contains(id), "id {id} must be in list_memories");
            }

            // --- Embedding consistency: batch == single-add for this build ---
            // Both paths call open_embedder_for once. In lean builds both
            // produce NULL (Noop). In the embeddings build both produce a real
            // model string. Confirm all batch rows share the same model as the
            // reference single-add row.
            let ref_id = add_memory(
                &root,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "single-add reference for embedding consistency check",
            )
            .expect("single add ref");
            {
                let (_paths, _config, conn) = load_project(&root).expect("load project");
                let ref_model: Option<String> = conn
                    .query_row(
                        "SELECT embedding_model FROM memories WHERE memory_id = ?1",
                        rusqlite::params![ref_id],
                        |r| r.get(0),
                    )
                    .expect("query ref embedding_model");

                // All batch-added memories must have the same embedding_model.
                for id in &ids {
                    let bm: Option<String> = conn
                        .query_row(
                            "SELECT embedding_model FROM memories WHERE memory_id = ?1",
                            rusqlite::params![id],
                            |r| r.get(0),
                        )
                        .expect("query batch embedding_model");
                    assert_eq!(
                        bm, ref_model,
                        "batch memory {id} embedding_model ({bm:?}) must match single-add ref ({ref_model:?})"
                    );
                }
            }

            // --- Survive rebuild_in_place ------------------------------------
            // After rebuild: 5 batch + 1 single-add = 6 active memories.
            {
                let (_paths, _config, conn) = load_project(&root).expect("load for rebuild");
                crate::projector::rebuild_in_place(&conn).expect("rebuild_in_place");
                let after_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memories \
                         WHERE invalidated_at IS NULL AND superseded_by IS NULL",
                        [],
                        |r| r.get(0),
                    )
                    .expect("count after rebuild");
                assert_eq!(
                    after_count, 6,
                    "all 6 memories (5 batch + 1 single) must survive rebuild_in_place; got {after_count}"
                );
                let rebuilt_ids: Vec<String> = {
                    let mut stmt = conn
                        .prepare(
                            "SELECT memory_id FROM memories \
                             WHERE invalidated_at IS NULL AND superseded_by IS NULL",
                        )
                        .expect("prepare");
                    stmt.query_map([], |r| r.get(0))
                        .expect("query")
                        .map(|r| r.expect("row"))
                        .collect()
                };
                for id in &ids {
                    assert!(
                        rebuilt_ids.contains(id),
                        "id {id} must survive rebuild_in_place"
                    );
                }
            }

            // --- Temporal fields (valid_from / valid_to) survive rebuild -----
            let temporal_entries = vec![BatchMemoryEntry {
                text: "batch temporal test this fact expires soon".to_string(),
                scope: kimetsu_core::memory::MemoryScope::Project,
                kind: kimetsu_core::memory::MemoryKind::Fact,
                valid_from: Some("2025-01-01T00:00:00Z".to_string()),
                valid_to: Some("2099-12-31T00:00:00Z".to_string()),
            }];
            let temporal_ids =
                add_memories_batch(&root, temporal_entries).expect("add_memories_batch temporal");
            assert_eq!(temporal_ids.len(), 1);
            let temporal_id = &temporal_ids[0];
            {
                let (_paths, _config, conn) = load_project(&root).expect("load for temporal check");
                let (vf, vt): (Option<String>, Option<String>) = conn
                    .query_row(
                        "SELECT valid_from, valid_to FROM memories WHERE memory_id = ?1",
                        rusqlite::params![temporal_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .expect("query valid_from/valid_to");
                assert!(
                    vf.is_some(),
                    "valid_from must be set for temporal batch entry"
                );
                assert!(
                    vt.is_some(),
                    "valid_to must be set for temporal batch entry"
                );
                // Survive rebuild.
                crate::projector::rebuild_in_place(&conn).expect("rebuild temporal");
                let (vf2, vt2): (Option<String>, Option<String>) = conn
                    .query_row(
                        "SELECT valid_from, valid_to FROM memories WHERE memory_id = ?1",
                        rusqlite::params![temporal_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .expect("query after rebuild");
                assert_eq!(vf, vf2, "valid_from must survive rebuild");
                assert_eq!(vt, vt2, "valid_to must survive rebuild");
            }

            fs::remove_dir_all(&root).ok();
        });
    }

    /// Dedup: calling add_memories_batch with the same text twice must return
    /// the same memory_id both times without writing a duplicate row.
    #[test]
    fn add_memories_batch_deduplicates() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            let text = "batch dedup test unique entry";
            let entries = vec![
                BatchMemoryEntry {
                    text: text.to_string(),
                    scope: kimetsu_core::memory::MemoryScope::Project,
                    kind: kimetsu_core::memory::MemoryKind::Fact,
                    valid_from: None,
                    valid_to: None,
                },
                BatchMemoryEntry {
                    text: text.to_string(),
                    scope: kimetsu_core::memory::MemoryScope::Project,
                    kind: kimetsu_core::memory::MemoryKind::Fact,
                    valid_from: None,
                    valid_to: None,
                },
            ];

            let ids = add_memories_batch(&root, entries).expect("add_memories_batch dedup");
            assert_eq!(ids.len(), 2);
            assert_eq!(
                ids[0], ids[1],
                "duplicate text must return the same memory_id"
            );

            // Only one row in the DB.
            let memories = list_memories(&root).expect("list");
            assert_eq!(
                memories.len(),
                1,
                "deduped batch must produce exactly 1 DB row; got {}",
                memories.len()
            );

            fs::remove_dir_all(&root).ok();
        });
    }

    /// Embedder-loaded-once structural check: add_memories_batch calls
    /// open_embedder_for exactly once before the loop. This test confirms that
    /// all batch-added memories have the same embedding_model value — a
    /// necessary condition for single-load: if the embedder were re-initialized
    /// per entry, different initializations could produce different model ids.
    ///
    /// In the lean build all entries have NULL embedding_model (Noop).
    /// In the embeddings build all entries share the same real model id.
    /// Either way: all N values are identical.
    #[test]
    fn add_memories_batch_all_entries_same_embedding_model() {
        with_user_brain_disabled(|| {
            let root = test_root();
            fs::create_dir_all(&root).expect("create temp project");
            init_project(&root, false).expect("init project");

            let n = 8_usize;
            let entries: Vec<BatchMemoryEntry> = (0..n)
                .map(|i| BatchMemoryEntry {
                    text: format!(
                        "embedding model consistency test memory {i} distinct content here"
                    ),
                    scope: kimetsu_core::memory::MemoryScope::Project,
                    kind: kimetsu_core::memory::MemoryKind::Convention,
                    valid_from: None,
                    valid_to: None,
                })
                .collect();

            let ids = add_memories_batch(&root, entries).expect("add_memories_batch");
            assert_eq!(ids.len(), n);

            let (_paths, _config, conn) = load_project(&root).expect("load project");
            let model_id_rows: Vec<Option<String>> = {
                let mut stmt = conn
                    .prepare("SELECT embedding_model FROM memories ORDER BY created_at")
                    .expect("prepare");
                stmt.query_map([], |r| r.get(0))
                    .expect("query")
                    .map(|r| r.expect("row"))
                    .collect()
            };
            assert_eq!(model_id_rows.len(), n, "expected {n} rows");
            // All entries must share the same embedding_model value (even if NULL).
            let first = &model_id_rows[0];
            for (i, model_id) in model_id_rows.iter().enumerate() {
                assert_eq!(
                    model_id, first,
                    "memory {i} embedding_model ({model_id:?}) must match first ({first:?})"
                );
            }

            fs::remove_dir_all(&root).ok();
        });
    }
}
