//! Embeddings + hybrid retrieval scaffolding (v0.4.2).
//!
//! The broker is FTS-only through v0.4.1: right answers with wrong
//! words get missed. v0.4.2 lays the infrastructure for hybrid
//! retrieval (lexical + semantic) without binding to a specific
//! embedder. v0.4.3 wires fastembed-rs as the production default.
//!
//! Layers introduced here:
//!   1. The [`Embedder`] trait — anything that can map a text to a
//!      fixed-dimension float vector plus identify the model that
//!      produced it.
//!   2. [`NoopEmbedder`] — production default when no real embedder
//!      is wired. `embed()` errors with `NotImplemented`; the write
//!      path treats that as "store NULL" and the retrieval path
//!      treats it as "skip the cosine blend, FTS only".
//!   3. [`StubEmbedder`] — deterministic, dependency-free, test-only
//!      pseudo-embedder. Lets us exercise the hybrid scoring path
//!      end-to-end without depending on fastembed-rs or downloading
//!      a model in CI.
//!   4. [`cosine_similarity`] + BLOB codec helpers so the brain.db
//!      schema can store embeddings as little-endian `f32` blobs.
//!
//! Wire compatibility:
//!   * Embeddings are nullable. Pre-v0.4.2 rows have NULL embedding
//!     + NULL embedding_model. The retrieval blender treats them as
//!       "lexical-only" — they still score via FTS, they just don't
//!       contribute to the cosine term.
//!   * The `embedding_model` column carries an opaque string id
//!     ("bge-small-en-v1.5", "stub-d8", etc.). Queries blend only
//!     when the query's embedder id matches the row's stored id,
//!     so mixing models inside one brain.db is safe (rows with a
//!     different model fall back to lexical-only).
//!
//! Scoring (added in v0.4.2):
//!   `final_relevance = (1 - alpha) * lexical + alpha * cosine`
//!   where `alpha = brain.broker.hybrid_alpha` (defaulted to 0.5,
//!   tuned later in v0.4.3 after live measurements). When no
//!   cosine signal exists, `alpha = 0` effectively — i.e. pure
//!   lexical. See [`context::memory_candidates`] for the wiring.

use kimetsu_core::KimetsuResult;

/// Default blend factor for hybrid scoring. 0.0 = pure lexical
/// (v0.4.1 behavior), 1.0 = pure cosine. v0.4.2 ships 0.5 as a
/// starting point; live data in v0.4.3 will tune it.
pub const DEFAULT_HYBRID_ALPHA: f32 = 0.5;

/// Embedder trait. Every implementation maps a text to a fixed
/// dimension `Vec<f32>` and identifies itself via a stable model id.
///
/// `Send + Sync` so a single embedder instance can be shared across
/// the chat REPL's threads (drainer, REPL, hook runner) without
/// requiring per-call locking.
pub trait Embedder: Send + Sync {
    /// Compute an embedding for `text`. The returned vector MUST have
    /// length == `self.dim()`. Implementations may normalize.
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedderError>;

    /// Stable identifier for the model used. Stored alongside each
    /// embedding so retrieval can detect cross-model mismatches and
    /// skip the cosine blend rather than comparing apples-to-oranges.
    fn model_id(&self) -> &str;

    /// Embedding dimension. Used by the BLOB codec to validate
    /// stored vectors against the active model on retrieval.
    fn dim(&self) -> usize;

    /// Convenience: true when this embedder is the production no-op.
    /// Callers can short-circuit the write/retrieval cosine path
    /// instead of allocating a vec only to discard it.
    fn is_noop(&self) -> bool {
        false
    }
}

/// Implement `Embedder` for `Box<dyn Embedder>` so callers can hold
/// an owned trait object without ceremony.
impl Embedder for Box<dyn Embedder> {
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedderError> {
        (**self).embed(text)
    }
    fn model_id(&self) -> &str {
        (**self).model_id()
    }
    fn dim(&self) -> usize {
        (**self).dim()
    }
    fn is_noop(&self) -> bool {
        (**self).is_noop()
    }
}

/// Failure modes for an embedder.
#[derive(Debug, Clone)]
pub enum EmbedderError {
    /// The embedder is intentionally a no-op — no embeddings will be
    /// produced. Callers should fall back to a NULL embedding /
    /// lexical-only retrieval.
    NotImplemented,
    /// Model failed to load. v0.4.3+ — e.g. the fastembed backend
    /// can't download the model.
    LoadFailed(String),
    /// Inference failed (rare).
    EmbedFailed(String),
    /// Dimension mismatch between the embedder and a stored row.
    /// The retrieval path skips this row's cosine contribution.
    DimMismatch { expected: usize, got: usize },
}

impl std::fmt::Display for EmbedderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented => write!(f, "embedder not implemented"),
            Self::LoadFailed(msg) => write!(f, "embedder load failed: {msg}"),
            Self::EmbedFailed(msg) => write!(f, "embed call failed: {msg}"),
            Self::DimMismatch { expected, got } => {
                write!(f, "embedding dim mismatch: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for EmbedderError {}

/// Production default when no real embedder is configured.
/// `embed()` returns `Err(NotImplemented)`; callers interpret that
/// as "store NULL" / "skip the cosine blend".
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEmbedder;

impl NoopEmbedder {
    pub const MODEL_ID: &'static str = "noop";
}

impl Embedder for NoopEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedderError> {
        Err(EmbedderError::NotImplemented)
    }

    fn model_id(&self) -> &str {
        Self::MODEL_ID
    }

    fn dim(&self) -> usize {
        0
    }

    fn is_noop(&self) -> bool {
        true
    }
}

/// Deterministic, dependency-free pseudo-embedder used in tests.
///
/// Hashes each word into a fixed-dim bucket (count-of-hash-buckets),
/// then L2-normalizes the resulting vector. NOT semantic — texts
/// that share words will be close; texts that don't share words
/// will be far. Good enough to exercise the hybrid-scoring code
/// path without depending on a real ML model.
///
/// Default dimension is 8 (small enough to keep tests fast).
#[derive(Debug, Clone, Copy)]
pub struct StubEmbedder {
    dim: usize,
}

impl StubEmbedder {
    pub const MODEL_ID: &'static str = "stub-d8";

    pub const fn new() -> Self {
        Self { dim: 8 }
    }

    pub const fn with_dim(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for StubEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder for StubEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedderError> {
        let mut bucket = vec![0.0f32; self.dim];
        for word in text.split_whitespace() {
            // Cheap, stable hash. Don't use DefaultHasher — its seed
            // randomizes across processes and tests would be flaky.
            // FNV-1a over the lowercased UTF-8 bytes is plenty.
            let normalized = word.to_lowercase();
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in normalized.bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01B3);
            }
            let idx = (h as usize) % self.dim.max(1);
            bucket[idx] += 1.0;
        }
        // L2-normalize so cosine similarity reduces to a dot product.
        let norm = bucket.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut bucket {
                *v /= norm;
            }
        }
        Ok(bucket)
    }

    fn model_id(&self) -> &str {
        Self::MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// Open the production-default embedder.
///
/// Resolution (v0.4.3):
///   1. `KIMETSU_BRAIN_EMBEDDER=noop|off|none` → always `NoopEmbedder`,
///      regardless of Cargo features. Useful for CI, hooks, and
///      transient subprocesses that shouldn't pay the model-load
///      cost.
///   2. Cargo feature `embeddings` enabled →
///      [`fastembed_backend::open_cached`] returns a process-wide
///      cached [`FastembedEmbedder`] for the model picked by
///      [`pick_builtin_model_from_env`] (default `bge-small-en-v1.5`,
///      `bge-m3` or `jina-v2-base-code` opt-in via env). On model
///      load failure (network, disk, ort runtime missing) we log
///      and fall through to Noop so the brain stays usable on FTS
///      alone.
///   3. Cargo feature `embeddings` disabled → `NoopEmbedder`,
///      identical to v0.4.2 build.
///
/// The returned trait object is borrowed from a process-static
/// `OnceLock`; production callers get model-load cost paid exactly
/// once over the process lifetime. Tests that need a different
/// embedder must use [`crate::context::retrieve_context_with_embedder`]
/// with an explicit [`StubEmbedder`] (or any other [`Embedder`])
/// instead of going through this function.
pub fn open_default_embedder() -> &'static (dyn Embedder + Send + Sync) {
    static CACHE: std::sync::OnceLock<Box<dyn Embedder + Send + Sync>> = std::sync::OnceLock::new();
    let embedder = CACHE.get_or_init(build_default_embedder);
    embedder.as_ref()
}

fn build_default_embedder() -> Box<dyn Embedder + Send + Sync> {
    if env_disables_embedder() {
        return Box::new(NoopEmbedder);
    }
    #[cfg(feature = "embeddings")]
    {
        match fastembed_backend::open_cached() {
            Ok(handle) => return Box::new(handle),
            Err(err) => {
                eprintln!(
                    "kimetsu-brain: fastembed init failed ({err}); falling back to NoopEmbedder. \
                     Retrieval will stay FTS-only this session. Re-run with \
                     KIMETSU_BRAIN_EMBEDDER=noop to silence this warning."
                );
            }
        }
    }
    Box::new(NoopEmbedder)
}

/// W3.1: config-aware embedder resolver.
///
/// Returns the shared real embedder when embeddings are enabled, or a
/// static [`NoopEmbedder`] when they are disabled — so a project with
/// `[embedder] enabled = false` gets FTS-only retrieval and writes no
/// vectors, durably, without relying on the `KIMETSU_BRAIN_EMBEDDER`
/// env var.
///
/// Precedence mirrors [`embedder_enabled_for_config`]:
///   1. `KIMETSU_BRAIN_EMBEDDER` env disable value → Noop.
///   2. `KIMETSU_BRAIN_EMBEDDER` real model id → real embedder.
///   3. Env unset → `config_enabled` governs.
pub fn open_embedder_for(config_enabled: bool) -> &'static dyn Embedder {
    if embedder_enabled_for_config(config_enabled) {
        open_default_embedder()
    } else {
        &NoopEmbedder
    }
}

/// v0.8: open a FRESH (uncached) embedder for an explicit built-in
/// model id. Unlike [`open_default_embedder`], this bypasses the
/// process-static cache AND the env/override resolution — the caller
/// asked for a *specific* model (e.g. `kimetsu brain model set` and the
/// MCP `model_set` reindex, which must re-embed with the newly-chosen
/// model even though the running process may have a different default
/// embedder cached). Returns [`NoopEmbedder`] on the lean build or if
/// the model fails to load.
pub fn open_embedder_for_model(model_id: &str) -> Box<dyn Embedder + Send + Sync> {
    #[cfg(feature = "embeddings")]
    {
        match fastembed_backend::FastembedEmbedder::try_open(model_id) {
            Ok(engine) => return Box::new(engine),
            Err(err) => {
                eprintln!(
                    "kimetsu-brain: failed to open embedder `{model_id}` ({err}); \
                     using NoopEmbedder (no vectors produced)."
                );
            }
        }
    }
    #[cfg(not(feature = "embeddings"))]
    {
        let _ = model_id;
    }
    Box::new(NoopEmbedder)
}

/// v0.4.3: env-driven kill switch. Truthy values (1/true/yes/on)
/// force-disable the embedder for this process; "noop", "off",
/// "none" do the same. Anything else (or unset) leaves the
/// `embeddings` feature in control.
fn env_disables_embedder() -> bool {
    match std::env::var("KIMETSU_BRAIN_EMBEDDER") {
        Ok(value) => is_disable_value(&value.trim().to_ascii_lowercase()),
        Err(_) => false,
    }
}

/// W3.1: config-aware enabled check. Resolution precedence:
///   1. `KIMETSU_BRAIN_EMBEDDER` env is set to a disable value → false.
///   2. `KIMETSU_BRAIN_EMBEDDER` env is set to a real model id → true
///      (explicit model override = caller wants embeddings on).
///   3. Env is unset → `config_enabled` governs.
///
/// Keep the env-only `env_disables_embedder()` working for back-compat
/// callers (the `OnceLock` path).
pub fn embedder_enabled_for_config(config_enabled: bool) -> bool {
    // Precedence: env override > config > default.
    match std::env::var("KIMETSU_BRAIN_EMBEDDER") {
        Ok(raw) => {
            let v = raw.trim().to_ascii_lowercase();
            if v.is_empty() {
                // Empty string — treat as unset, fall through to config.
                config_enabled
            } else if is_disable_value(&v) {
                // Explicit disable in env wins.
                false
            } else {
                // A real model id in env = caller wants embeddings on.
                true
            }
        }
        // Env unset → config governs.
        Err(_) => config_enabled,
    }
}

fn is_disable_value(v: &str) -> bool {
    matches!(v, "noop" | "off" | "none" | "0" | "false" | "no")
}

/// v0.8: curated built-in embedding models, surfaced by
/// `kimetsu brain model list` and `kimetsu_brain_model_list`. Tuple
/// = (stable id, vector dimension, human blurb). This is the single
/// source of truth for the selectable set; the fastembed backend
/// maps these ids → `EmbeddingModel` in `try_open`.
pub const BUILTIN_MODELS: &[(&str, usize, &str)] = &[
    ("bge-small-en-v1.5", 384, "English, default, ~67 MB int8"),
    ("bge-m3", 1024, "Multilingual, ~600 MB int8"),
    (
        "jina-v2-base-code",
        768,
        "English + code-tuned, ~165 MB int8",
    ),
];

/// v0.8: process-global embedder override recorded by
/// [`apply_embedder_selection`]. Brain-internal callers
/// ([`pick_builtin_model_from_env`], the fastembed backend, reindex)
/// have no `ProjectConfig` in hand, so the CLI/MCP layer stashes the
/// config-selected id here once, early, before any embed happens.
static EMBEDDER_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// v0.8: record the config-provided embedder id so brain-internal
/// callers resolve it when `KIMETSU_BRAIN_EMBEDDER` is unset (the env
/// var always wins). Call once, early, before the first retrieval or
/// embed — after the embedder `OnceLock` initializes this has no
/// effect. No-op for `None`/empty, and only the first call sticks.
pub fn apply_embedder_selection(config_embedder: Option<&str>) {
    if let Some(id) = config_embedder {
        let id = id.trim();
        if !id.is_empty() {
            let _ = EMBEDDER_OVERRIDE.set(id.to_string());
        }
    }
}

/// v0.8: map any accepted alias (env value, config value, built-in
/// id) to a stable built-in id. Unknown values warn and fall back to
/// the lean English default. Disable values map to the default too;
/// the *actual* disable is handled separately by
/// [`env_disables_embedder`].
fn map_builtin_id(v: &str) -> &'static str {
    match v {
        "" | "default" | "bge-small" | "bge-small-en-v1.5" => "bge-small-en-v1.5",
        "bge-m3" | "m3" => "bge-m3",
        "jina-code" | "jina-v2-base-code" | "jina-embeddings-v2-base-code" => "jina-v2-base-code",
        "noop" | "off" | "none" | "0" | "false" | "no" => "bge-small-en-v1.5",
        other => {
            eprintln!(
                "kimetsu-brain: unknown embedder {other:?}, \
                 falling back to bge-small-en-v1.5"
            );
            "bge-small-en-v1.5"
        }
    }
}

/// v0.8: resolve the active built-in model id. Precedence:
///   1. `KIMETSU_BRAIN_EMBEDDER` env (unless it's a disable value)
///   2. the explicit `config_embedder` arg, else the override set by
///      [`apply_embedder_selection`]
///   3. `bge-small-en-v1.5` default
pub fn resolve_embedder_id(config_embedder: Option<&str>) -> &'static str {
    if let Ok(raw) = std::env::var("KIMETSU_BRAIN_EMBEDDER") {
        let v = raw.trim().to_ascii_lowercase();
        if !v.is_empty() && !is_disable_value(&v) {
            return map_builtin_id(&v);
        }
        // empty / disable values fall through: the model *id* still
        // resolves from config/default even when retrieval is off.
    }
    let cfg = config_embedder
        .map(str::to_string)
        .or_else(|| EMBEDDER_OVERRIDE.get().cloned());
    if let Some(c) = cfg {
        let v = c.trim().to_ascii_lowercase();
        if !v.is_empty() {
            return map_builtin_id(&v);
        }
    }
    "bge-small-en-v1.5"
}

/// v0.4.3: pick which builtin model to load from the env, returning
/// a stable identifier. Used both by the fastembed backend (to map
/// id → `EmbeddingModel`) and by `kimetsu brain reindex` (to label
/// new rows with the right `embedding_model`).
///
/// Resolution:
///   * unset / "" / "default" / "bge-small" / "bge-small-en-v1.5"
///     → `"bge-small-en-v1.5"` (384 dim, ~67 MB int8, English)
///   * "bge-m3"
///     → `"bge-m3"` (1024 dim, ~600 MB int8, multilingual)
///   * "jina-code" / "jina-v2-base-code" /
///     "jina-embeddings-v2-base-code"
///     → `"jina-v2-base-code"` (768 dim, ~165 MB int8, English +
///     code-tuned)
///   * anything else falls back to bge-small with a warning.
pub fn pick_builtin_model_from_env() -> &'static str {
    // v0.8: env > config-override (set via `apply_embedder_selection`)
    // > default. Kept as a named entry point for the fastembed backend
    // and `reindex`, which have no `ProjectConfig` to pass.
    resolve_embedder_id(None)
}

// v0.4.3: real fastembed-backed embedder. Lives behind the
// `embeddings` Cargo feature so the default build skips the
// ~50-transitive-crate dep tree (ONNX runtime, tokenizers, etc).
#[cfg(feature = "embeddings")]
mod fastembed_backend {
    use super::{Embedder, EmbedderError, pick_builtin_model_from_env};
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use std::sync::{Arc, Mutex, OnceLock};

    /// fastembed-backed embedder. Wraps the ONNX runtime in a
    /// `Mutex` because `TextEmbedding::embed` takes `&mut self`.
    /// The lock window is short (one inference per call); the
    /// chat REPL's threads serialize cleanly through it.
    pub struct FastembedEmbedder {
        model_id: &'static str,
        dim: usize,
        engine: Mutex<TextEmbedding>,
    }

    impl FastembedEmbedder {
        pub fn try_open(builtin_id: &str) -> Result<Self, EmbedderError> {
            let (kind, model_id, dim) = match builtin_id {
                "bge-m3" => (EmbeddingModel::BGEM3, "bge-m3", 1024),
                "jina-v2-base-code" => (
                    EmbeddingModel::JinaEmbeddingsV2BaseCode,
                    "jina-v2-base-code",
                    768,
                ),
                // bge-small-en-v1.5 is the default + fallback.
                _ => (EmbeddingModel::BGESmallENV15, "bge-small-en-v1.5", 384),
            };
            let opts = InitOptions::new(kind).with_show_download_progress(false);
            let engine = TextEmbedding::try_new(opts)
                .map_err(|e| EmbedderError::LoadFailed(format!("fastembed init: {e}")))?;
            Ok(Self {
                model_id,
                dim,
                engine: Mutex::new(engine),
            })
        }
    }

    impl Embedder for FastembedEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedderError> {
            let mut guard = self
                .engine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut out = guard
                .embed(vec![text], None)
                .map_err(|e| EmbedderError::EmbedFailed(format!("fastembed embed: {e}")))?;
            let vec = out
                .pop()
                .ok_or_else(|| EmbedderError::EmbedFailed("empty result".into()))?;
            if vec.len() != self.dim {
                return Err(EmbedderError::DimMismatch {
                    expected: self.dim,
                    got: vec.len(),
                });
            }
            Ok(vec)
        }

        fn model_id(&self) -> &str {
            self.model_id
        }

        fn dim(&self) -> usize {
            self.dim
        }
    }

    /// Shared handle. `open_default_embedder` boxes this into a
    /// `dyn Embedder` and stashes it in a process-static `OnceLock`,
    /// so we only call into ONNX once per process.
    #[derive(Clone)]
    pub struct EmbedderHandle(Arc<FastembedEmbedder>);

    impl Embedder for EmbedderHandle {
        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedderError> {
            self.0.embed(text)
        }
        fn model_id(&self) -> &str {
            self.0.model_id()
        }
        fn dim(&self) -> usize {
            self.0.dim()
        }
    }

    /// Open (or return the cached) fastembed embedder for the model
    /// picked by `KIMETSU_BRAIN_EMBEDDER`. Errors here propagate up
    /// to `open_default_embedder`, which falls back to Noop +
    /// prints a one-line warning.
    pub fn open_cached() -> Result<EmbedderHandle, EmbedderError> {
        static CELL: OnceLock<Result<Arc<FastembedEmbedder>, EmbedderError>> = OnceLock::new();
        let init = CELL.get_or_init(|| {
            let builtin = pick_builtin_model_from_env();
            FastembedEmbedder::try_open(builtin).map(Arc::new)
        });
        match init {
            Ok(arc) => Ok(EmbedderHandle(arc.clone())),
            Err(err) => Err(err.clone()),
        }
    }
}

// --------- math helpers ---------

/// Cosine similarity between two vectors. Returns 0.0 when either
/// vector is empty or all-zeros. Does NOT assume the vectors are
/// pre-normalized — divides by both norms.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// --------- write-path helper ---------

/// Compute the embedding for `text` and persist it onto an existing
/// `memories.memory_id` row.
///
/// No-op when the embedder is intentionally a [`NoopEmbedder`] (it
/// returns [`EmbedderError::NotImplemented`] which we silently
/// swallow — the column stays NULL, retrieval falls back to
/// FTS-only for the row, exact v0.4.1 behavior).
///
/// For other embedder errors we surface them up. The caller
/// (`add_memory`, `add_user_memory`) can decide whether to fail the
/// whole insert or log+continue — today they propagate.
///
/// Returns the computed embedding vector (so callers can reuse it
/// for conflict detection without re-embedding). Returns `None` on
/// Noop or NotImplemented — the same cases where the column stays NULL.
pub fn embed_and_persist(
    conn: &rusqlite::Connection,
    memory_id: &str,
    text: &str,
    embedder: &dyn Embedder,
) -> KimetsuResult<Option<Vec<f32>>> {
    if embedder.is_noop() {
        return Ok(None);
    }
    let vec = match embedder.embed(text) {
        Ok(v) => v,
        // NotImplemented is the contract for "skip silently". Treat
        // any embedder that signals it the same way as NoopEmbedder.
        Err(EmbedderError::NotImplemented) => return Ok(None),
        Err(e) => return Err(format!("embed failed for memory {memory_id}: {e}").into()),
    };
    if vec.len() != embedder.dim() {
        return Err(format!(
            "embedder {} produced {} dims, expected {}",
            embedder.model_id(),
            vec.len(),
            embedder.dim()
        )
        .into());
    }
    let blob = encode_embedding(&vec);
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE memory_id = ?3",
        rusqlite::params![blob, embedder.model_id(), memory_id],
    )?;

    // Fix 4b: maintain memory_vec incrementally at add time so retrieval
    // never needs to do the O(N) cold backfill for newly-added rows.
    // Only compiled on embeddings builds (vec0 is not available on lean).
    #[cfg(feature = "embeddings")]
    {
        // ensure_vec_table is cheap DDL-only (no backfill scan).
        // upsert_vec_row is a single O(1) INSERT OR REPLACE.
        // Errors here are best-effort: a vec0 insert failure must not abort
        // a successful memory write. The next retrieval's ensure_vec_index
        // backfill will catch any missing rows.
        if let Err(e) = crate::context::ensure_vec_table(conn, embedder.model_id(), embedder.dim())
        {
            eprintln!(
                "kimetsu-brain: vec table DDL failed for memory {memory_id}: {e} (vec index will backfill on next retrieval)"
            );
        } else if let Err(e) = crate::context::upsert_vec_row(conn, memory_id, &blob) {
            eprintln!(
                "kimetsu-brain: vec row upsert failed for memory {memory_id}: {e} (vec index will backfill on next retrieval)"
            );
        }
    }

    Ok(Some(vec))
}

// --------- BLOB codec ---------
//
// Embeddings are stored as little-endian f32 BLOBs. The encoder
// fixes byte order so brain.db files move between architectures.
// The decoder is strict: it returns Err if the byte length isn't a
// multiple of 4, or if the resulting dim doesn't match expectations.

/// Serialize a float vector to little-endian bytes for storage.
pub fn encode_embedding(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Decode a BLOB back into a float vector. Optionally validates the
/// expected dimension; pass `None` to accept any length.
pub fn decode_embedding(bytes: &[u8], expected_dim: Option<usize>) -> KimetsuResult<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return Err(format!("embedding blob length {} not a multiple of 4", bytes.len()).into());
    }
    let dim = bytes.len() / 4;
    if let Some(expected) = expected_dim
        && dim != expected
    {
        return Err(format!("embedding blob dim {dim} does not match expected {expected}").into());
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in bytes.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_builtin_id_maps_aliases_and_defaults_unknown() {
        assert_eq!(map_builtin_id("bge-small-en-v1.5"), "bge-small-en-v1.5");
        assert_eq!(map_builtin_id("default"), "bge-small-en-v1.5");
        assert_eq!(map_builtin_id("m3"), "bge-m3");
        assert_eq!(map_builtin_id("bge-m3"), "bge-m3");
        assert_eq!(map_builtin_id("jina-code"), "jina-v2-base-code");
        assert_eq!(
            map_builtin_id("jina-embeddings-v2-base-code"),
            "jina-v2-base-code"
        );
        // disable values resolve to the lean default (the kill-switch
        // is handled separately by env_disables_embedder).
        assert_eq!(map_builtin_id("noop"), "bge-small-en-v1.5");
        // unknown -> warn + default.
        assert_eq!(map_builtin_id("totally-made-up"), "bge-small-en-v1.5");
    }

    #[test]
    fn builtin_models_table_is_consistent() {
        // Every advertised id must map back to itself.
        for (id, _dim, _blurb) in BUILTIN_MODELS {
            assert_eq!(map_builtin_id(id), *id, "id {id} must be stable");
        }
    }

    #[test]
    fn resolve_embedder_id_uses_config_when_env_unset() {
        // Env mutation in parallel tests is racy + unsafe in edition
        // 2024, so we only assert the config/default paths when the env
        // var is genuinely absent. The env-wins path is exercised
        // manually (see the plan's verification section).
        if std::env::var_os("KIMETSU_BRAIN_EMBEDDER").is_some() {
            return;
        }
        assert_eq!(resolve_embedder_id(Some("bge-m3")), "bge-m3");
        assert_eq!(resolve_embedder_id(Some("jina-code")), "jina-v2-base-code");
        // unknown config value -> default.
        assert_eq!(resolve_embedder_id(Some("nope")), "bge-small-en-v1.5");
        // None + no override stored in this test binary -> default.
        assert_eq!(resolve_embedder_id(None), "bge-small-en-v1.5");
    }

    #[test]
    fn noop_embedder_returns_not_implemented_and_is_noop() {
        let e = NoopEmbedder;
        assert!(e.is_noop());
        assert_eq!(e.dim(), 0);
        assert_eq!(e.model_id(), "noop");
        assert!(matches!(
            e.embed("hello").unwrap_err(),
            EmbedderError::NotImplemented
        ));
    }

    #[test]
    fn stub_embedder_is_deterministic() {
        let e = StubEmbedder::new();
        let a = e.embed("hello rust").expect("embed a");
        let b = e.embed("hello rust").expect("embed b");
        let c = e.embed("hello RUST").expect("embed c");
        assert_eq!(a, b, "same input -> same output");
        assert_eq!(
            a, c,
            "lowercasing means case differences collapse to the same vector"
        );
        assert_eq!(a.len(), 8);
        // L2-normalized: norm == 1 within float tolerance.
        let norm = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[test]
    fn stub_embedder_distinguishes_disjoint_inputs() {
        let e = StubEmbedder::new();
        let a = e.embed("foo bar").expect("a");
        let b = e.embed("qux quux").expect("b");
        let sim = cosine_similarity(&a, &b);
        // Disjoint word sets *can* still collide in the 8-bucket
        // hash, but on average should be low. Sanity bound: not 1.0.
        assert!(
            sim < 0.99,
            "disjoint inputs should not be near-identical: {sim}"
        );
    }

    #[test]
    fn stub_embedder_handles_empty_input() {
        let e = StubEmbedder::new();
        let v = e.embed("").expect("empty embed");
        assert_eq!(v.len(), 8);
        // All zeros: cosine similarity with self is 0 (we guard against
        // division by zero), which is exactly the behavior the
        // retrieval blender wants for content-free queries.
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn cosine_similarity_handles_edge_cases() {
        // Identical normalized vectors -> 1.0.
        let a = [1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);

        // Orthogonal -> 0.0.
        let b = [0.0f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);

        // Anti-parallel -> -1.0.
        let c = [-1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &c) + 1.0).abs() < 1e-6);

        // Empty / mismatched dim -> 0.0 by contract.
        assert_eq!(cosine_similarity(&[], &a), 0.0);
        assert_eq!(cosine_similarity(&a, &[0.0]), 0.0);

        // Zero norm -> 0.0 by contract (don't divide by zero).
        let zeros = [0.0f32, 0.0, 0.0];
        assert_eq!(cosine_similarity(&zeros, &a), 0.0);
    }

    #[test]
    fn cosine_similarity_is_symmetric() {
        let a = [0.6f32, 0.8, 0.0];
        let b = [0.0f32, 1.0, 0.0];
        let ab = cosine_similarity(&a, &b);
        let ba = cosine_similarity(&b, &a);
        assert!((ab - ba).abs() < 1e-6);
        // Dot is 0.8, |a|=1, |b|=1 -> sim = 0.8.
        assert!((ab - 0.8).abs() < 1e-5);
    }

    #[test]
    fn encode_decode_embedding_round_trip() {
        let vec = vec![0.1f32, -0.2, 3.125, -0.000_001, 42.0];
        let blob = encode_embedding(&vec);
        assert_eq!(blob.len(), vec.len() * 4);
        let back = decode_embedding(&blob, Some(vec.len())).expect("decode");
        assert_eq!(back.len(), vec.len());
        for (orig, got) in vec.iter().zip(back.iter()) {
            assert!(
                (orig - got).abs() < 1e-7,
                "f32 round-trip should be bit-exact"
            );
        }
    }

    #[test]
    fn decode_embedding_rejects_unaligned_blob() {
        let bad = [0u8, 1, 2]; // 3 bytes - not a multiple of 4
        let err = decode_embedding(&bad, None).unwrap_err();
        assert!(err.to_string().contains("not a multiple of 4"));
    }

    #[test]
    fn decode_embedding_rejects_dim_mismatch() {
        let vec = vec![1.0f32, 2.0, 3.0];
        let blob = encode_embedding(&vec);
        let err = decode_embedding(&blob, Some(5)).unwrap_err();
        assert!(err.to_string().contains("does not match expected"));
    }

    /// v0.4.3: under the default Cargo build (no `embeddings` feature)
    /// `open_default_embedder` MUST return Noop so a `cargo install
    /// kimetsu-cli` user doesn't accidentally start downloading a
    /// model from $HOME. Skip when `--features embeddings` is on —
    /// that build path has its own integration tests (run with
    /// `cargo test --features embeddings -- --ignored`).
    #[cfg(not(feature = "embeddings"))]
    #[test]
    fn open_default_embedder_returns_noop_on_default_build() {
        let e = open_default_embedder();
        assert!(e.is_noop());
        assert_eq!(e.dim(), 0);
        assert!(matches!(
            e.embed("anything").unwrap_err(),
            EmbedderError::NotImplemented
        ));
    }

    /// v0.4.3: env kill-switch works even when the `embeddings`
    /// feature is on — `KIMETSU_BRAIN_EMBEDDER=noop` returns Noop
    /// regardless. Tests the env parser directly rather than going
    /// through the cached `open_default_embedder`, which would
    /// otherwise be poisoned by whatever the previous test in the
    /// process initialized.
    #[test]
    fn env_disables_embedder_recognizes_off_values() {
        let lock = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
        for value in ["noop", "off", "NONE", "0", "false", "no"] {
            // SAFETY: serialized via the shared brain test env lock.
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_EMBEDDER", value);
            }
            assert!(env_disables_embedder(), "value {value:?} must disable");
        }
        for value in ["", "default", "bge-small", "bge-m3", "jina-code"] {
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_EMBEDDER", value);
            }
            assert!(!env_disables_embedder(), "value {value:?} must NOT disable");
        }
        // Restore.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
            }
        }
        drop(lock);
    }

    // ── W3.1: embedder_enabled_for_config tests ──────────────────────

    /// W3.1: config=false disables embedder when env is unset.
    #[test]
    fn w3_embedder_enabled_for_config_false_when_env_unset() {
        let lock = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
        unsafe {
            std::env::remove_var("KIMETSU_BRAIN_EMBEDDER");
        }
        // config=false + env unset → disabled.
        assert!(
            !embedder_enabled_for_config(false),
            "config=false + env unset must be disabled"
        );
        // config=true + env unset → enabled (default).
        assert!(
            embedder_enabled_for_config(true),
            "config=true + env unset must be enabled"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
            }
        }
        drop(lock);
    }

    /// W3.1: KIMETSU_BRAIN_EMBEDDER=noop overrides config=true.
    #[test]
    fn w3_embedder_env_disable_overrides_config_true() {
        let lock = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
        unsafe {
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "noop");
        }
        assert!(
            !embedder_enabled_for_config(true),
            "KIMETSU_BRAIN_EMBEDDER=noop must override config=true"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
            }
        }
        drop(lock);
    }

    /// W3.1: a real model-id in env overrides config=false (explicit
    /// model = caller wants embeddings on).
    #[test]
    fn w3_embedder_env_model_id_overrides_config_false() {
        let lock = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
        unsafe {
            std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "bge-m3");
        }
        assert!(
            embedder_enabled_for_config(false),
            "real model id in env must override config=false → enabled"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
            }
        }
        drop(lock);
    }

    /// v0.4.3: model picker maps the user-facing env string onto a
    /// stable model id used by both fastembed init AND the
    /// `embedding_model` column on each memory row.
    #[test]
    fn pick_builtin_model_from_env_handles_aliases() {
        let lock = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_BRAIN_EMBEDDER").ok();
        let cases = [
            ("", "bge-small-en-v1.5"),
            ("default", "bge-small-en-v1.5"),
            ("bge-small", "bge-small-en-v1.5"),
            ("BGE-SMALL-EN-V1.5", "bge-small-en-v1.5"),
            ("bge-m3", "bge-m3"),
            ("M3", "bge-m3"),
            ("jina-code", "jina-v2-base-code"),
            ("jina-v2-base-code", "jina-v2-base-code"),
            ("jina-embeddings-v2-base-code", "jina-v2-base-code"),
            // Unknown values fall back to bge-small with a warning.
            ("totally-made-up", "bge-small-en-v1.5"),
        ];
        for (input, expected) in cases {
            // SAFETY: serialized via the shared brain test env lock.
            unsafe {
                std::env::set_var("KIMETSU_BRAIN_EMBEDDER", input);
            }
            assert_eq!(
                pick_builtin_model_from_env(),
                expected,
                "input {input:?} -> expected {expected}"
            );
        }
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_BRAIN_EMBEDDER", v),
                None => std::env::remove_var("KIMETSU_BRAIN_EMBEDDER"),
            }
        }
        drop(lock);
    }
}
