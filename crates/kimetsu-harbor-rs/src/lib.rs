//! Terminal-Bench / Harbor transport adapter for kimetsu.
//!
//! v0.3 architectural intent: keep the Harbor / Terminal-Bench specific
//! JSON-RPC transport out of `kimetsu-agent` so other transports
//! (the new `kimetsu-chat` REPL, future HTTP / MCP servers, embedded
//! library use) don't pay the cost of dragging in benchmark-only code.
//!
//! For v0.3.0 this crate is intentionally a thin re-export shell over
//! `kimetsu_agent::harbor`. The HarborSession + JSON-RPC types still
//! live in `kimetsu-agent/src/harbor.rs` because they're entangled
//! with the agent loop's call sites and a Big Bang move would risk
//! breaking the production benchmark binary that's currently shipping.
//!
//! Phase-2 work (slated for v0.3.1+): physically migrate the JSON-RPC
//! layer here, leaving `kimetsu-agent` with the protocol-agnostic
//! tool runtime, prompts, verify loop, and provider plumbing.
//!
//! Public API:
//!   - re-export of the harbor entry point used by `kimetsu-cli`
//!     (`agent --harbor-mode`)
//!   - re-export of the model-agent loop + opts struct (for any external
//!     consumer that wants to drive a harbor session programmatically)
//!
//! Why a separate crate now (even as a re-export) instead of waiting?
//!   - Lets us version harbor independently of the agent core.
//!   - Lets `kimetsu-chat` depend ONLY on `kimetsu-agent`, never on
//!     harbor — verifying the dependency direction we want before
//!     the physical migration lands.
//!   - Makes the architectural intent legible from the workspace
//!     layout alone.

// Core transport types — currently still defined in kimetsu-agent::harbor.
// Phase-2 will physically move these here.
pub use kimetsu_agent::harbor::{
    AgentDoneParams, HARBOR_PROTOCOL_VERSION, HarborAgentOpts, HarborRequest, HarborSession,
    HarborShellExecutor, JsonRpcError, JsonRpcResponse, ModelAgentReport, ToolExecParams,
    ToolExecResult, run_model_agent, run_stub_agent,
};

// Tool definitions + dispatcher live with the agent runtime, but harbor
// callers need them to drive the agent loop. Re-exporting them here keeps
// kimetsu-cli's harbor-mode entry point dependency-clean.
pub use kimetsu_agent::harbor::{harbor_dispatch_tool, harbor_tool_definitions};

// Defaults the harbor runner cares about.
pub use kimetsu_agent::harbor::{
    DEFAULT_MODEL_TURN_BUDGET, MAX_VERIFY_ITERATIONS, STALL_WINDOW_TURNS,
};

/// Crate-level metadata so harbor-specific telemetry can distinguish
/// "harbor mode" from any future transport without sniffing call sites.
pub const TRANSPORT_NAME: &str = "harbor";
pub const TRANSPORT_VERSION: &str = env!("CARGO_PKG_VERSION");
