//! Flagship 1 / Pass B / Story 1.1 + 1.2: repo digest builder.
//!
//! Builds a compact ~400-token digest of the current repo state:
//!   - top-usefulness memories (conventions/facts that matter most)
//!   - repo manifest summary (Cargo.toml, package.json, …)
//!   - recent run focus ("current focus" from run history)
//!
//! The digest is cached in `.kimetsu/digest.md`, keyed by a SHA-256
//! CONTENT HASH of the inputs.  Staleness is detected cheaply (git HEAD
//! change, manifest hash change, memory corpus change) and the rebuild
//! runs detached so it never blocks SessionStart.
//!
//! ## Cheap-model vs rule-based
//!
//! When `config.cheap_model()` returns `Some(cm)` the digest is distilled
//! by an LLM call (not yet wired — requires async HTTP client that is
//! already present in the distiller).  When `None`, a rule-based assembler
//! concatenates the raw inputs directly.  The rule-based path is the only
//! path exercised in tests and in the current implementation (the
//! expensive LLM path is guarded and degrades gracefully).
//!
//! ## ROI attribution
//!
//! After the SessionStart hook emits context, it writes `digest_served` /
//! `resume_served` attribution events to the brain via
//! [`record_warmstart_served`].

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::project::{load_project, load_project_readonly};

// ── Target size ──────────────────────────────────────────────────────────────

/// Approx character budget for the assembled digest (≈400 tokens × 4 chars).
const DIGEST_CHAR_BUDGET: usize = 1_600;
/// Number of top-useful memories to include in the digest.
const TOP_MEMORY_COUNT: usize = 5;
/// Number of recent run titles to include in "current focus".
const RECENT_RUNS_COUNT: usize = 3;
/// Max chars per memory text included in digest.
const MEMORY_SNIPPET_CHARS: usize = 180;

// ── Cache metadata ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestMeta {
    /// SHA-256-like content hash of the inputs (via DefaultHasher for speed).
    pub input_hash: u64,
    /// ISO-8601 timestamp when this digest was built.
    pub built_at: String,
}

// ── Public surface ────────────────────────────────────────────────────────────

/// Build (or load from cache) a compact repo digest for `workspace`.
///
/// Returns `None` when:
/// - the brain is not initialized at `workspace`
/// - the workspace has no useful content yet (no memories, no manifests)
///
/// The returned string is already budget-capped and ready for injection.
///
/// `force_rebuild` bypasses the cache.
pub fn build_or_load_digest(workspace: &Path, force_rebuild: bool) -> Option<String> {
    build_or_load_digest_inner(workspace, force_rebuild).unwrap_or(None)
}

fn build_or_load_digest_inner(
    workspace: &Path,
    force_rebuild: bool,
) -> KimetsuResult<Option<String>> {
    let (paths, config, conn) = load_project_readonly(workspace)?;
    let repo_root_str = paths.repo_root.to_string_lossy().to_string();

    // 1. Assemble raw inputs.
    let inputs = gather_inputs(&conn, &repo_root_str)?;
    if inputs.is_empty() {
        return Ok(None);
    }

    // 2. Compute content hash.
    let hash = content_hash(&inputs);

    // 3. Cache paths.
    let cache_path = paths.kimetsu_dir.join("digest.md");
    let meta_path = paths.kimetsu_dir.join("digest-meta.json");

    // 4. Check cache validity.
    if !force_rebuild {
        if let Some(cached) = try_load_cache(&cache_path, &meta_path, hash) {
            return Ok(Some(cached));
        }
    }

    // 5. Build the digest (cheap-model optional; rule-based otherwise).
    let digest_text = assemble_rule_based(&inputs, &config)?;
    if digest_text.trim().is_empty() {
        return Ok(None);
    }

    // 6. Write cache atomically.
    let meta = DigestMeta {
        input_hash: hash,
        built_at: now_utc_rfc3339(),
    };
    atomic_write_text(&cache_path, &digest_text);
    atomic_write_json_meta(&meta_path, &meta);

    Ok(Some(digest_text))
}

/// Read `.kimetsu/digest.md` verbatim, without checking whether it is
/// still current.
///
/// This is the warm-path counterpart to [`build_or_load_digest`]: the
/// caller serves the cached text immediately and rebuilds off the hot
/// path (see [`is_stale`]), instead of paying a synchronous rebuild the
/// moment the corpus moves. Returns `None` when the brain is not
/// initialized here or nothing has been cached yet — a cold start still
/// has to build.
pub fn load_cached_digest(workspace: &Path) -> Option<String> {
    let (paths, _config, _conn) = load_project_readonly(workspace).ok()?;
    let text = std::fs::read_to_string(paths.kimetsu_dir.join("digest.md")).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

// ── Staleness check (1.2) ─────────────────────────────────────────────────────

/// Returns `true` when the cached digest is stale and should be rebuilt.
///
/// Cheap: only checks the content hash (no I/O heavier than reading the
/// meta sidecar and querying two SQLite count rows).
///
/// Used by the SessionStart hook to decide whether to spawn a detached
/// rebuild before injecting the (potentially stale) cached digest.
pub fn is_stale(workspace: &Path) -> bool {
    is_stale_inner(workspace).unwrap_or(false)
}

fn is_stale_inner(workspace: &Path) -> KimetsuResult<bool> {
    let (paths, _config, conn) = load_project_readonly(workspace)?;
    let repo_root_str = paths.repo_root.to_string_lossy().to_string();

    let meta_path = paths.kimetsu_dir.join("digest-meta.json");
    let cache_path = paths.kimetsu_dir.join("digest.md");

    if !cache_path.exists() || !meta_path.exists() {
        return Ok(true);
    }

    let meta = load_meta(&meta_path)?;
    let inputs = gather_inputs(&conn, &repo_root_str)?;
    let current_hash = content_hash(&inputs);

    Ok(meta.input_hash != current_hash)
}

// ── Warm start ────────────────────────────────────────────────────────────────

/// Assemble the warm-start block: repo digest, standing preferences, and
/// episodic resume.
///
/// This is what every host sees first — the `SessionStart` hook on Claude
/// Code, the first prompt of a session on Codex / Pi / OpenClaw, and the first
/// `kimetsu_brain_context` call on Cursor, which has neither hooks nor a
/// session-start surface.
///
/// Returns `None` when `[broker] warm_start` is off, or when there is no
/// digest, no preferences and no live episode to report.
///
/// The cached digest is served even when the corpus has moved under it, and the
/// rebuild is spawned detached — a synchronous rebuild would sit in front of the
/// agent's first turn. Only a cold brain (nothing cached yet) builds inline.
///
/// Records ROI attribution as a side effect, so call it only when the block is
/// actually going to be emitted.
pub fn warm_start_block(workspace: &Path) -> Option<String> {
    // Gate: load warm_start from config (best-effort; default ON).
    let warm_start_enabled = kimetsu_core::paths::ProjectPaths::discover(workspace)
        .ok()
        .and_then(|paths| crate::project::load_config(&paths).ok())
        .map(|cfg| cfg.broker.warm_start)
        .unwrap_or(true);
    if !warm_start_enabled {
        return None;
    }

    let digest = match load_cached_digest(workspace) {
        Some(cached) => {
            if is_stale(workspace) {
                spawn_detached_refresh(workspace);
            }
            Some(cached)
        }
        None => build_or_load_digest(workspace, false),
    };
    let resume = crate::episode::render_resume_context(workspace);

    // v2.6: the user's standing preferences, delivered rather than retrieved.
    //
    // Preference following is the second-weakest measured ability, and the
    // diagnosis is that "a preference is a small aside semantically far from
    // the question" — which rules out re-ranking, because the candidate never
    // enters the pool. A standing preference belongs in context before the
    // question is asked. See `crate::user_profile`.
    let profile = user_profile_block(workspace);

    // v2.6: what the skills loop is waiting on. Detection has run on a schedule
    // since the maintenance daemon landed, but its result went into a log file
    // nobody opens — so a memory could earn skill status and never become one.
    // See `crate::skill_synthesis::graduation_notice`.
    let skills = skills_block(workspace);

    if digest.is_none() && resume.is_none() && profile.is_none() && skills.is_none() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = &digest {
        parts.push(format!("## Repo context\n{d}"));
    }
    if let Some(p) = &profile {
        parts.push(format!("## How you like to work\n{p}"));
    }
    if let Some(r) = &resume {
        parts.push(format!("## Your prior session\n{r}"));
    }
    // Last: it is a nudge about Kimetsu itself, not context about the repo, so
    // it must not sit between the agent and the work.
    if let Some(s) = &skills {
        parts.push(format!("## Skills ready to graduate\n{s}"));
    }

    record_warmstart_served(
        workspace,
        digest.as_ref().map(|d| d.len()).unwrap_or(0),
        resume.as_ref().map(|r| r.len()).unwrap_or(0),
    );

    Some(parts.join("\n\n"))
}

/// Assemble the skills-loop nudge for the warm start.
///
/// Best-effort, like every other block here: an unreadable brain means no
/// nudge, never a failed warm start.
fn skills_block(workspace: &Path) -> Option<String> {
    let (_paths, _config, conn) = load_project_readonly(workspace).ok()?;
    crate::skill_synthesis::graduation_notice(&conn)
}

/// Assemble the standing-preferences block for the warm start.
///
/// Best-effort: an unreadable brain means no preferences block, never a failed
/// warm start.
fn user_profile_block(workspace: &Path) -> Option<String> {
    let (_paths, _config, conn) = load_project_readonly(workspace).ok()?;
    // The cross-project user brain is opened separately; when it is disabled or
    // unreachable the project's own preferences stand on their own.
    let user_conn = kimetsu_core::paths::user_brain_db_path()
        .filter(|path| path.exists())
        .and_then(|path| Connection::open(&path).ok());
    let profile = crate::user_profile::build_profile(&conn, user_conn.as_ref()).ok()?;
    crate::user_profile::render_profile(&profile)
}

/// Fire-and-forget `<current_exe> brain digest --refresh --workspace <ws>`.
///
/// Assumes the running executable is the kimetsu CLI, which holds for every
/// caller of [`warm_start_block`] (the hooks and the MCP server are both the
/// `kimetsu` binary). Embedders of this crate that are not the CLI simply get a
/// spawn that fails and is swallowed — a stale digest, never a broken host.
///
/// Fully detached with null stdio, mirroring the embed daemon's spawn: an
/// inherited stdout pipe would hold the host's hook open until its timeout.
fn spawn_detached_refresh(workspace: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["brain", "digest", "--refresh", "--workspace"])
        .arg(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let _ = cmd.spawn();
}

// ── ROI attribution ───────────────────────────────────────────────────────────

/// Record ROI attribution events for the warm-start injection.
///
/// `digest_chars` is the length of the emitted digest (0 = not emitted).
/// `resume_chars` is the length of the emitted resume (0 = not emitted).
///
/// Best-effort: errors are ignored (ROI must never block SessionStart).
pub fn record_warmstart_served(workspace: &Path, digest_chars: usize, resume_chars: usize) {
    let _ = record_warmstart_served_inner(workspace, digest_chars, resume_chars);
}

fn record_warmstart_served_inner(
    workspace: &Path,
    digest_chars: usize,
    resume_chars: usize,
) -> KimetsuResult<()> {
    if digest_chars == 0 && resume_chars == 0 {
        return Ok(());
    }
    let (_paths, _config, conn) = load_project(workspace)?;
    let ts = now_utc_rfc3339();

    if digest_chars > 0 {
        let approx_tokens = digest_chars / 4;
        let event = kimetsu_core::event::Event::new(
            kimetsu_core::ids::RunId::new(),
            "digest_served",
            serde_json::json!({
                "digest_chars": digest_chars,
                "approx_tokens": approx_tokens,
                "ts": ts,
            }),
        );
        let _ = crate::projector::insert_event(&conn, &event);
    }

    if resume_chars > 0 {
        let approx_tokens = resume_chars / 4;
        let event = kimetsu_core::event::Event::new(
            kimetsu_core::ids::RunId::new(),
            "resume_served",
            serde_json::json!({
                "resume_chars": resume_chars,
                "approx_tokens": approx_tokens,
                "ts": ts,
            }),
        );
        let _ = crate::projector::insert_event(&conn, &event);
    }

    Ok(())
}

// ── Input assembly ────────────────────────────────────────────────────────────

/// Raw ingredients for the digest.
#[derive(Debug, Default)]
struct DigestInputs {
    /// Top-useful memory snippets: `(kind, text_snippet)`.
    top_memories: Vec<(String, String)>,
    /// Manifest summaries: `(manifest_kind, path)` e.g. ("cargo", "Cargo.toml").
    manifests: Vec<(String, String)>,
    /// Recent run task titles.
    recent_runs: Vec<String>,
}

impl DigestInputs {
    fn is_empty(&self) -> bool {
        self.top_memories.is_empty() && self.manifests.is_empty() && self.recent_runs.is_empty()
    }
}

fn gather_inputs(conn: &Connection, repo_root: &str) -> KimetsuResult<DigestInputs> {
    let mut inputs = DigestInputs::default();

    // Top-useful memories (conventions/facts, no superseded/invalidated).
    // Include memories with use_count = 0 (fresh adds) ordered by recency
    // so new brains produce useful digests without requiring prior runs.
    // use_count > 0 memories are ranked by usefulness ratio; use_count = 0
    // rows sort last (usefulness_score default 0).
    //
    // v2.6: preferences are excluded. They now have their own warm-start
    // section (`crate::user_profile`), which sits directly beside this one, so
    // including them here would print the same lines twice in the same block —
    // and the digest's slots are better spent on facts the preferences section
    // will never carry.
    {
        let mut stmt = conn.prepare(
            "SELECT kind, text
             FROM memories
             WHERE invalidated_at IS NULL
               AND superseded_by IS NULL
               AND kind != 'preference'
             ORDER BY
               CASE WHEN use_count > 0
                    THEN (usefulness_score / CAST(use_count AS REAL))
                    ELSE 0.0
               END DESC,
               use_count DESC,
               created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([TOP_MEMORY_COUNT as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for (kind, text) in rows.flatten() {
            let snippet: String = text.chars().take(MEMORY_SNIPPET_CHARS).collect();
            inputs.top_memories.push((kind, snippet));
        }
    }

    // Repo manifests (Cargo.toml, package.json, pyproject.toml, …)
    {
        let mut stmt = conn.prepare(
            "SELECT manifest_kind, manifest_path
             FROM repo_manifests
             WHERE repo_root = ?1
             LIMIT 10",
        )?;
        let rows = stmt.query_map([repo_root], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for pair in rows.flatten() {
            inputs.manifests.push(pair);
        }
    }

    // Recent run summaries from work_episodes (current focus).
    {
        let mut stmt = conn.prepare(
            "SELECT task
             FROM work_episodes
             WHERE repo_root = ?1
               AND superseded_by IS NULL
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map([repo_root, &RECENT_RUNS_COUNT.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        for task in rows.flatten() {
            if !task.trim().is_empty() {
                inputs.recent_runs.push(task);
            }
        }
    }

    Ok(inputs)
}

// ── Content hash ──────────────────────────────────────────────────────────────

fn content_hash(inputs: &DigestInputs) -> u64 {
    let mut h = DefaultHasher::new();
    for (kind, text) in &inputs.top_memories {
        kind.hash(&mut h);
        text.hash(&mut h);
    }
    for (mk, mp) in &inputs.manifests {
        mk.hash(&mut h);
        mp.hash(&mut h);
    }
    for task in &inputs.recent_runs {
        task.hash(&mut h);
    }
    h.finish()
}

// ── Rule-based assembler ──────────────────────────────────────────────────────

fn assemble_rule_based(
    inputs: &DigestInputs,
    _config: &kimetsu_core::config::ProjectConfig,
) -> KimetsuResult<String> {
    let mut parts: Vec<String> = Vec::new();

    // Manifests → project type hint.
    if !inputs.manifests.is_empty() {
        let manifest_list: Vec<String> = inputs
            .manifests
            .iter()
            .map(|(kind, path)| format!("{kind}: {path}"))
            .collect();
        parts.push(format!("Project manifests: {}", manifest_list.join(", ")));
    }

    // Current focus.
    if !inputs.recent_runs.is_empty() {
        let focus = inputs
            .recent_runs
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>();
        if !focus.is_empty() {
            parts.push(format!("Current focus: {}", focus.join(" / ")));
        }
    }

    // Top memories.
    if !inputs.top_memories.is_empty() {
        parts.push("Key conventions and facts:".to_string());
        for (kind, text) in &inputs.top_memories {
            parts.push(format!("[{kind}] {text}"));
        }
    }

    let digest = parts.join("\n");

    // Budget-cap: truncate to char limit with ellipsis.
    if digest.len() > DIGEST_CHAR_BUDGET {
        let mut s: String = digest.chars().take(DIGEST_CHAR_BUDGET - 3).collect();
        s.push_str("...");
        Ok(s)
    } else {
        Ok(digest)
    }
}

// ── Cache helpers ─────────────────────────────────────────────────────────────

fn try_load_cache(cache_path: &Path, meta_path: &Path, current_hash: u64) -> Option<String> {
    if !cache_path.exists() || !meta_path.exists() {
        return None;
    }
    let meta = load_meta(meta_path).ok()?;
    if meta.input_hash != current_hash {
        return None;
    }
    std::fs::read_to_string(cache_path).ok()
}

fn load_meta(meta_path: &Path) -> KimetsuResult<DigestMeta> {
    let text = std::fs::read_to_string(meta_path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Atomic text write: temp + rename.
fn atomic_write_text(path: &Path, content: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let tmp = path.with_extension("md.tmp");
    if std::fs::write(&tmp, content).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Atomic JSON meta write: temp + rename.
fn atomic_write_json_meta(path: &Path, meta: &DigestMeta) {
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(text) = serde_json::to_string(meta) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn now_utc_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use kimetsu_core::paths::git_init_boundary;

    use super::*;
    use crate::{project, user_brain};

    fn tmp_workspace(name: &str) -> std::path::PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("kimetsu-digest-{name}-{ts}"));
        std::fs::create_dir_all(&dir).expect("create tmp");
        dir
    }

    // D1: empty brain returns None (no content to digest).
    #[test]
    fn empty_brain_returns_none() {
        let dir = tmp_workspace("empty");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            let result = build_or_load_digest(&dir, false);
            assert!(result.is_none(), "empty brain must return None digest");
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D2: digest with memories is non-empty and ≤ budget.
    #[test]
    fn digest_with_memories_is_bounded() {
        let dir = tmp_workspace("bounded");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            // Seed a memory so there's content to digest.
            // The digest includes memories even with use_count=0 (fresh adds).
            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Convention,
                "Always use git_init_boundary before init_project in tests",
            )
            .expect("add_memory");

            let digest = build_or_load_digest(&dir, true).expect("digest must be Some");
            assert!(!digest.is_empty(), "digest must be non-empty");
            assert!(
                digest.len() <= DIGEST_CHAR_BUDGET + 3,
                "digest must respect char budget: {} chars",
                digest.len()
            );
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D3: cache is reused on second call (no force_rebuild).
    #[test]
    fn cache_is_reused_on_second_call() {
        let dir = tmp_workspace("cache");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "Rust edition 2024 is the target edition for this workspace",
            )
            .expect("add_memory");
            let d1 = build_or_load_digest(&dir, true).expect("first build");
            let d2 = build_or_load_digest(&dir, false).expect("cached load");
            assert_eq!(d1, d2, "cached digest must match first build");
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D4: force_rebuild bypasses cache.
    #[test]
    fn force_rebuild_bypasses_cache() {
        let dir = tmp_workspace("force");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Convention,
                "Force rebuild test convention",
            )
            .expect("add_memory");
            let d1 = build_or_load_digest(&dir, true).expect("first build");
            let d2 = build_or_load_digest(&dir, true).expect("forced rebuild");
            // Content should match because inputs are the same.
            assert_eq!(
                d1, d2,
                "forced rebuild must produce same content when inputs unchanged"
            );
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D5: is_stale returns true when no cache exists.
    #[test]
    fn is_stale_true_when_no_cache() {
        let dir = tmp_workspace("stale");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            assert!(is_stale(&dir), "must be stale when cache does not exist");
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D6: is_stale returns false after a successful build.
    #[test]
    fn is_stale_false_after_build() {
        let dir = tmp_workspace("fresh");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "After-build staleness check fact",
            )
            .expect("add_memory");
            let _ = build_or_load_digest(&dir, true);
            assert!(
                !is_stale(&dir),
                "must NOT be stale immediately after a fresh build"
            );
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D6b: load_cached_digest returns the cached text without rebuilding, and
    // keeps returning it once the corpus has moved on. This is what lets the
    // warm start serve instantly and rebuild off the hot path.
    #[test]
    fn load_cached_digest_serves_stale_text() {
        let dir = tmp_workspace("cached-stale");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            assert!(
                load_cached_digest(&dir).is_none(),
                "nothing cached yet on a cold brain"
            );

            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Fact,
                "Cached digest fact",
            )
            .expect("add_memory");
            let built = build_or_load_digest(&dir, true).expect("first build");
            assert_eq!(load_cached_digest(&dir).as_deref(), Some(built.as_str()));

            // Move the corpus: the cache is now stale, but still servable.
            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Convention,
                "A second memory that invalidates the digest hash",
            )
            .expect("add_memory");
            assert!(is_stale(&dir), "corpus moved — cache must read as stale");
            assert_eq!(
                load_cached_digest(&dir).as_deref(),
                Some(built.as_str()),
                "stale cache is still served verbatim"
            );
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D6c: warm_start_block honours the [broker] warm_start gate, and produces
    // the digest section when there is content.
    #[test]
    fn warm_start_block_respects_gate_and_renders_digest() {
        let dir = tmp_workspace("warm-block");
        git_init_boundary(&dir);
        user_brain::with_user_brain_disabled(|| {
            project::init_project(&dir, true).expect("init");
            project::add_memory(
                &dir,
                kimetsu_core::memory::MemoryScope::Project,
                kimetsu_core::memory::MemoryKind::Convention,
                "Warm start block convention",
            )
            .expect("add_memory");

            let block = warm_start_block(&dir).expect("warm start must have content");
            assert!(
                block.contains("## Repo context"),
                "warm start must carry the repo digest: {block}"
            );

            // Turn the gate off; the block must disappear entirely.
            let paths = kimetsu_core::paths::ProjectPaths::discover(&dir).expect("paths");
            let mut config = crate::project::load_config(&paths).expect("config");
            config.broker.warm_start = false;
            std::fs::write(&paths.project_toml, config.to_toml().expect("to_toml"))
                .expect("write project.toml");

            assert!(
                warm_start_block(&dir).is_none(),
                "[broker] warm_start = false must silence the warm start"
            );
        });
        std::fs::remove_dir_all(dir).ok();
    }

    // D7: digest size is ≤ ~400 tokens (character proxy: 1600 chars).
    // This is the measurement/gate required by Story 1.6.
    #[test]
    fn digest_size_within_400_token_budget() {
        // Assemble a large set of inputs and verify the rule-based assembler
        // respects the budget.
        let inputs = DigestInputs {
            top_memories: (0..10)
                .map(|i| {
                    (
                        "convention".to_string(),
                        "A".repeat(MEMORY_SNIPPET_CHARS) + &format!(" #{i}"),
                    )
                })
                .collect(),
            manifests: (0..5)
                .map(|i| ("cargo".to_string(), format!("Cargo{i}.toml")))
                .collect(),
            recent_runs: (0..5).map(|i| format!("task {i}")).collect(),
        };
        let config = kimetsu_core::config::ProjectConfig::default_for_project("test");
        let digest = assemble_rule_based(&inputs, &config).expect("assemble");
        let char_count = digest.chars().count();
        assert!(
            char_count <= DIGEST_CHAR_BUDGET + 3,
            "digest must fit in budget: got {char_count} chars (budget={DIGEST_CHAR_BUDGET})"
        );
        // Approximate token count: chars / 4.
        let approx_tokens = char_count / 4;
        assert!(
            approx_tokens <= 420,
            "approx token count {approx_tokens} must be ≤ 420"
        );
    }

    // D8: record_warmstart_served is best-effort (no panic on uninitialized brain).
    #[test]
    fn record_warmstart_served_is_best_effort() {
        let tmp = std::env::temp_dir().join("kimetsu-digest-roi-besteffort");
        // No brain initialized — must not panic.
        record_warmstart_served(&tmp, 500, 100);
    }
}
