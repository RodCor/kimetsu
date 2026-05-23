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
//!     "lexical-only" — they still score via FTS, they just don't
//!     contribute to the cosine term.
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
/// v0.4.2: always [`NoopEmbedder`]. v0.4.3 will switch this to a
/// fastembed-backed BGE-small when the `embeddings` Cargo feature is
/// enabled (and back to Noop when it isn't).
pub fn open_default_embedder() -> Box<dyn Embedder> {
    Box::new(NoopEmbedder)
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
pub fn embed_and_persist(
    conn: &rusqlite::Connection,
    memory_id: &str,
    text: &str,
    embedder: &dyn Embedder,
) -> KimetsuResult<()> {
    if embedder.is_noop() {
        return Ok(());
    }
    let vec = match embedder.embed(text) {
        Ok(v) => v,
        // NotImplemented is the contract for "skip silently". Treat
        // any embedder that signals it the same way as NoopEmbedder.
        Err(EmbedderError::NotImplemented) => return Ok(()),
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
    Ok(())
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
    if !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "embedding blob length {} not a multiple of 4",
            bytes.len()
        )
        .into());
    }
    let dim = bytes.len() / 4;
    if let Some(expected) = expected_dim
        && dim != expected
    {
        return Err(format!(
            "embedding blob dim {dim} does not match expected {expected}"
        )
        .into());
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
        assert!(sim < 0.99, "disjoint inputs should not be near-identical: {sim}");
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
        let vec = vec![0.1f32, -0.2, 3.14, -0.000_001, 42.0];
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

    #[test]
    fn open_default_embedder_returns_noop_pre_v043() {
        // Until v0.4.3 wires fastembed, the default MUST be Noop so
        // production builds don't accidentally start downloading
        // models from the host's home dir.
        let e = open_default_embedder();
        assert!(e.is_noop());
        assert_eq!(e.dim(), 0);
        assert!(matches!(
            e.embed("anything").unwrap_err(),
            EmbedderError::NotImplemented
        ));
    }
}
