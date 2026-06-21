pub mod ambient;
pub mod analytics;
#[cfg(feature = "embeddings")]
pub mod ann;
/// S5.1: retrieval backend trait + FlatBackend implementation.
pub(crate) mod backend;
pub mod benchmark;
pub mod conflict;
pub mod consolidate;
pub mod context;
/// Flagship 1 / Pass B / Story 1.1+1.2+1.6: repo digest builder + cache + ROI.
pub mod digest;
pub mod dropped_capsule;
pub mod embeddings;
/// Flagship 1 / Story 1.3: episodic work-resume capture, storage, and surface.
pub mod episode;
pub mod eval;
pub mod ingest;
pub mod lock;
pub mod migrate;
pub mod project;
pub mod projector;
pub mod redact;
pub mod reindex;
pub mod roi;
pub mod schema;
pub mod skill_synthesis;
pub mod trace;
pub mod tune;
pub mod tuneset;
pub mod user_brain;
