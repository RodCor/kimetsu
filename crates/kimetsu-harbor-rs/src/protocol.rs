//! Kimetsu ↔ Harbor JSON-RPC protocol definitions.
//!
//! v0.3.2 — moved here from `kimetsu-agent::harbor`. The wire types
//! never belonged in the agent core; they only matter to the harbor
//! transport. Re-exports in `kimetsu-harbor-rs::lib` keep callers
//! (kimetsu-cli, the python adapter) on stable import paths.
//!
//! The line-oriented dialect:
//!
//! ```text
//! kimetsu -> harbor : {"jsonrpc":"2.0","id":N,"method":"tool.exec","params":{...}}
//! harbor  -> kimetsu: {"jsonrpc":"2.0","id":N,"result":{...}}
//! kimetsu -> harbor : {"jsonrpc":"2.0","method":"agent.done","params":{...}}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The wire-protocol version this crate speaks. Bumped when a non-
/// backward-compatible change to the message shape lands so the Python
/// adapter can refuse a mismatched kimetsu binary instead of silently
/// misbehaving.
pub const HARBOR_PROTOCOL_VERSION: &str = "0.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecParams {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Workspace-relative directory the command should run in. The
    /// adapter forwards this to `environment.exec(cwd=...)`. `None`
    /// means "wherever Harbor's environment defaults to" — typically
    /// the task root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Seconds. Adapter enforces the timeout if Harbor's environment
    /// doesn't, so a hung subprocess can't wedge the whole bench.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

pub(crate) fn default_timeout_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecResult {
    pub exit_code: i32,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    /// True iff the adapter (or Harbor itself) had to kill the process
    /// because the timeout fired. The kimetsu agent uses this to decide
    /// whether to retry-with-shorter-args or to surface the failure to
    /// the model.
    #[serde(default)]
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDoneParams {
    pub summary: String,
    /// Optional structured signal the adapter can fold into Harbor's
    /// AgentContext. Examples: {"final_patch": "..."} or
    /// {"verification_passed": true}. Kept open-ended for v0.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

/// A request kimetsu sends to the harness. Tagged by JSON-RPC `method`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum HarborRequest {
    #[serde(rename = "tool.exec")]
    ToolExec(ToolExecParams),
    #[serde(rename = "agent.done")]
    AgentDone(AgentDoneParams),
}

/// A response the harness sends back. Tagged by the presence of a
/// `result` or `error` field. A success carries `ToolExecResult`, a
/// failure carries a plain string message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}
