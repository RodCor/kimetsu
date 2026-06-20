//! S5.1: `RetrievalBackend` trait — the seam between candidate generation
//! and the broker.
//!
//! # Boundary
//!
//! The trait covers **memory candidate generation** only: given a query and an
//! optional pre-computed query embedding, produce the raw `Candidate` pool that
//! the broker (scoring, floors, rerank, compression) then operates on.
//!
//! Repo-file and manifest candidates are NOT part of the backend — they are
//! project-local and always generated the same way regardless of which backend
//! is active. This keeps the blast radius small: the broker is entirely
//! backend-agnostic.
//!
//! # Flat (current) behaviour
//!
//! [`FlatBackend`] is the first implementation. It is a pure refactor-in-place:
//! it delegates to the existing `context::memory_candidates` function, so the
//! FTS + usearch-ANN candidate path is UNCHANGED.
//!
//! # Future backends
//!
//! S5.2 slots `GraphLiteBackend` here. For S5.1 the config values
//! `"graph-lite"` and `"graph"` both resolve to [`FlatBackend`] so that
//! S5.1 ships with the selection point wired but graph traversal not yet
//! implemented — a TODO seam the next story fills in.

use rusqlite::Connection;

use kimetsu_core::KimetsuResult;

use crate::context::{Candidate, QueryEmbedding};

// ─── Trait ───────────────────────────────────────────────────────────────────

/// The retrieval backend trait: produces the **memory candidate pool** for a
/// given query.
///
/// All broker logic (lexical/semantic floors, scoring, MMR, compression) runs
/// ABOVE this trait and is backend-agnostic. Implementors only decide HOW to
/// surface the initial set of memory `Candidate`s — the broker takes it from
/// there.
///
/// The trait is `pub(crate)` because it is an internal architecture seam, not
/// a public API surface.
pub(crate) trait RetrievalBackend {
    /// Return the raw memory candidate pool for `query`.
    ///
    /// * `conn` — the brain SQLite connection to query.
    /// * `query` — the raw retrieval query string.
    /// * `query_embedding` — pre-computed query embedding, present when an
    ///   embedding model is active and successfully embedded the query. `None`
    ///   on lean (FTS-only) builds or when embedding failed silently.
    /// * `half_life_days` — usefulness-decay half-life from config; passed
    ///   through to `memory_row_to_candidate` for the decay multiplier.
    ///
    /// The returned slice is unsorted and unscored — the broker normalises and
    /// scores. Each element's `raw_relevance` carries the pre-normalisation
    /// signal (FTS BM25 blend or cosine blend) that the broker's per-kind max
    /// normalization uses.
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>>;
}

// ─── FlatBackend ─────────────────────────────────────────────────────────────

/// The flat (today's) retrieval backend.
///
/// Delegates directly to `context::memory_candidates`, which runs:
///   * On embeddings builds: FTS top-80 ∪ usearch-ANN top-80, merged by
///     memory-id (keeping the higher-scored instance).
///   * On lean builds: FTS top-80, falling back to latest-recency top-200
///     when FTS produces no results.
///
/// This is a pure refactor-in-place: identical SQL, identical ANN calls,
/// identical candidate set.
pub(crate) struct FlatBackend;

impl RetrievalBackend for FlatBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        crate::context::memory_candidates_flat(conn, query, query_embedding, half_life_days)
    }
}

// ─── Backend selection ───────────────────────────────────────────────────────

/// Resolve the configured backend variant name to a `Box<dyn RetrievalBackend>`.
///
/// Valid `backend` strings (from `[storage] backend = "…"` in project.toml):
///   * `"flat"` → [`FlatBackend`] (default, always available).
///   * `"graph-lite"` → [`FlatBackend`] (TODO seam — S5.2 slots the real impl).
///   * `"graph"` → [`FlatBackend`] (TODO seam — future story).
///   * Anything else → [`FlatBackend`] with an eprintln warning so a typo is
///     surfaced without crashing the process.
///
/// For S5.1 all variants resolve to `FlatBackend`. The `match` structure is
/// intentional: S5.2 replaces the `"graph-lite"` arm with `GraphLiteBackend`
/// without touching the rest of the function.
pub(crate) fn backend_for(backend: &str) -> Box<dyn RetrievalBackend + Send + Sync> {
    match backend {
        "flat" => Box::new(FlatBackend),
        "graph-lite" => {
            // S5.2: replace with GraphLiteBackend when implemented.
            Box::new(FlatBackend)
        }
        "graph" => {
            // Future story: full graph traversal backend.
            Box::new(FlatBackend)
        }
        other => {
            eprintln!(
                "kimetsu-brain: unknown storage.backend {:?}; falling back to \"flat\"",
                other
            );
            Box::new(FlatBackend)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// backend_for("flat") resolves to FlatBackend (smoke test — exercises the
    /// selection point without hitting SQLite).
    #[test]
    fn backend_for_flat_resolves() {
        let _b = backend_for("flat");
    }

    /// All variant strings resolve without panicking.
    #[test]
    fn backend_for_all_known_variants_no_panic() {
        for variant in &["flat", "graph-lite", "graph", "unknown-typo"] {
            let _b = backend_for(variant);
        }
    }
}
