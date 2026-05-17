//! Terminal-Bench / Harbor transport adapter for kimetsu.
//!
//! v0.3.2 — physical migration complete. The wire protocol +
//! HarborSession + HarborShellExecutor + stub runners that lived in
//! `kimetsu-agent/src/harbor.rs` through v0.3.1 are now physically
//! housed here. The agent core (`kimetsu-agent`) compiles + tests pass
//! without any reference to JSON-RPC machinery — exactly the
//! dependency property `kimetsu-chat` needed.
//!
//! Public modules:
//!   - [`protocol`] — wire-protocol type definitions (HarborRequest,
//!     ToolExecParams/Result, AgentDoneParams, JsonRpc*).
//!   - [`session`] — `HarborSession<R, W>` (the line-oriented RPC
//!     reader/writer) + `HarborShellExecutor` (the `ShellExecutor`
//!     impl that routes shell calls through the session).
//!   - [`stubs`] — `run_stub_agent` and `run_multi_step_stub` for the
//!     `kimetsu agent --harbor-mode --stub` smoke path used by the
//!     python adapter.
//!
//! Re-exports at the crate root surface the common types so callers
//! (kimetsu-cli, the python adapter via `kimetsu agent --harbor-mode`)
//! don't need to walk the module tree. Agent-core surface they also
//! need (run_model_agent, HarborAgentOpts, AgentDoneParams was already
//! here, etc.) is re-exported from `kimetsu_agent::harbor` for now;
//! Phase-3 (a future cleanup) will pull those agent-core re-exports
//! out of the harbor module entirely.

pub mod protocol;
pub mod session;
pub mod stubs;

// Wire-protocol surface.
pub use protocol::{
    AgentDoneParams, HARBOR_PROTOCOL_VERSION, HarborRequest, JsonRpcError, JsonRpcResponse,
    ToolExecParams, ToolExecResult,
};

// Session + executor surface.
pub use session::{HarborSession, HarborShellExecutor};

// Stub runners (for --stub mode).
pub use stubs::{MultiStepStubReport, run_multi_step_stub, run_stub_agent};

// Agent-core surface harbor callers still need. These live in
// kimetsu-agent because they're transport-agnostic; we re-export
// them here for convenience so kimetsu-cli has a single import path
// for everything harbor mode requires.
pub use kimetsu_agent::harbor::{
    DEFAULT_MODEL_TURN_BUDGET, HarborAgentOpts, MAX_VERIFY_ITERATIONS, ModelAgentReport,
    STALL_WINDOW_TURNS, harbor_dispatch_tool, harbor_tool_definitions, run_model_agent,
};

/// Crate-level metadata so harbor-specific telemetry can distinguish
/// "harbor mode" from any future transport without sniffing call sites.
pub const TRANSPORT_NAME: &str = "harbor";
pub const TRANSPORT_VERSION: &str = env!("CARGO_PKG_VERSION");
