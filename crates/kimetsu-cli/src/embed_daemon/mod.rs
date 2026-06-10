//! Warm embedder daemon: a per-user process that holds the ONNX embedding
//! model in memory and serves semantic retrieval to the (otherwise FTS-only)
//! `UserPromptSubmit` context-hook over a local socket / named pipe.
//!
//! All items are gated behind the `embeddings` feature — on lean builds there
//! is no daemon and the hook stays on floored-FTS.

#[cfg(feature = "embeddings")]
pub mod proto;

#[cfg(feature = "embeddings")]
pub mod ipc;

#[cfg(feature = "embeddings")]
pub mod server;

#[cfg(feature = "embeddings")]
pub mod client;

/// Bumped only on a wire-incompatible protocol change. Encoded into the
/// socket name so a new major routes to a fresh daemon.
#[cfg(feature = "embeddings")]
pub const PROTOCOL_MAJOR: u32 = 1;
