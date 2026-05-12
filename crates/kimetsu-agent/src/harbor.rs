//! MP-7a: Kimetsu ↔ Harbor JSON-RPC protocol + session.
//!
//! When kimetsu runs inside Harbor's external-agent harness (so that
//! Terminal-Bench can grade it), the kimetsu binary cannot run tools
//! locally — the workspace is a Docker container managed by Harbor and
//! the only legal way to touch it is through Harbor's
//! `environment.exec()` call. We bridge that gap with a tiny line-
//! oriented JSON-RPC dialect spoken on the binary's stdin/stdout:
//!
//! ```text
//! kimetsu -> harbor : {"jsonrpc":"2.0","id":N,"method":"tool.exec","params":{...}}
//! harbor  -> kimetsu: {"jsonrpc":"2.0","id":N,"result":{...}}
//! kimetsu -> harbor : {"jsonrpc":"2.0","method":"agent.done","params":{...}}
//! ```
//!
//! Only one tool is exposed in v0.2: `tool.exec(program, args, cwd,
//! timeout_secs)`. Read/write/diff/etc are all expressed as shell
//! commands (`cat`, `echo > file`, `git diff`), which mirrors how the
//! Harbor environment fundamentally works and keeps the protocol
//! surface trivial to maintain.
//!
//! For MP-7a this file ships the protocol + session primitives plus a
//! stub agent loop that:
//!   1. emits one `tool.exec` round-trip,
//!   2. emits `agent.done`,
//!   3. exits.
//! Real pipeline integration (broker + model + multi-step tool loop
//! flowing through HarborSession) lands in MP-7c, once MP-7b's Python
//! wrapper has been validated against Harbor.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use kimetsu_core::KimetsuResult;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The version this binary speaks. Bumped when the wire protocol changes
/// in a non-backward-compatible way so the Python adapter can refuse to
/// run a mismatched kimetsu binary instead of silently misbehaving.
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

fn default_timeout_secs() -> u64 {
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
/// `result` or `error` field. For MP-7a we keep it minimal: a success
/// carries `ToolExecResult`, a failure carries a plain string message.
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

/// Line-oriented JSON-RPC session. Reads one JSON object per line from
/// `reader`, writes one per line to `writer`. The `id` counter is
/// monotonic per session.
pub struct HarborSession<R: BufRead, W: Write> {
    reader: R,
    writer: W,
    next_id: AtomicU64,
}

impl<R: BufRead, W: Write> HarborSession<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            next_id: AtomicU64::new(1),
        }
    }

    /// Send a `tool.exec` request and block until the adapter answers
    /// with a matching id. Mismatched ids or missing results return an
    /// error so we never accidentally hand stale data to the model.
    pub fn request_tool_exec(&mut self, params: ToolExecParams) -> KimetsuResult<ToolExecResult> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tool.exec",
            "params": params,
        });
        self.write_frame(&frame)?;

        let response = self.read_response()?;
        if response.id != id {
            return Err(format!(
                "harbor adapter replied with id {} but kimetsu sent id {id}",
                response.id
            )
            .into());
        }
        if let Some(err) = response.error {
            return Err(format!(
                "harbor adapter returned error {}: {}",
                err.code, err.message
            )
            .into());
        }
        let result_value = response.result.ok_or_else(|| {
            "harbor adapter response missing both result and error".to_string()
        })?;
        let result: ToolExecResult = serde_json::from_value(result_value)
            .map_err(|err| format!("malformed tool.exec result: {err}"))?;
        Ok(result)
    }

    /// One-way notification: kimetsu has finished and the adapter can
    /// shut down. Per JSON-RPC convention this frame carries no `id`.
    pub fn emit_done(&mut self, params: AgentDoneParams) -> KimetsuResult<()> {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "agent.done",
            "params": params,
        });
        self.write_frame(&frame)
    }

    fn write_frame(&mut self, value: &Value) -> KimetsuResult<()> {
        let line = serde_json::to_string(value)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_response(&mut self) -> KimetsuResult<JsonRpcResponse> {
        let mut buf = String::new();
        let read = self.reader.read_line(&mut buf)?;
        if read == 0 {
            return Err("harbor adapter closed stdin before responding".into());
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            return Err("harbor adapter sent an empty line in place of a response".into());
        }
        let response: JsonRpcResponse = serde_json::from_str(trimmed)
            .map_err(|err| format!("malformed JSON-RPC frame from harbor adapter: {err}"))?;
        Ok(response)
    }
}

/// MP-7a stub agent loop. Exists so we can wire the CLI subcommand and
/// prove the protocol round-trips end to end before MP-7c plumbs the
/// real pipeline through. It runs one `echo` command via tool.exec,
/// emits agent.done, and returns.
pub fn run_stub_agent<R: BufRead, W: Write>(
    task: &str,
    session: &mut HarborSession<R, W>,
) -> KimetsuResult<ToolExecResult> {
    let probe = session.request_tool_exec(ToolExecParams {
        program: "echo".into(),
        args: vec![format!("kimetsu MP-7a harbor stub received task: {task}")],
        cwd: None,
        timeout_secs: default_timeout_secs(),
    })?;
    session.emit_done(AgentDoneParams {
        summary: format!(
            "stub agent for task `{task}` completed; protocol={HARBOR_PROTOCOL_VERSION}"
        ),
        context: Some(json!({
            "stub": true,
            "echo_exit_code": probe.exit_code,
        })),
    })?;
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Round-trip a single tool.exec: kimetsu writes the request,
    /// the harness "responds" with a canned ToolExecResult, kimetsu
    /// reads it back and surfaces it. Then verify the agent.done
    /// frame went out with the expected shape.
    #[test]
    fn harbor_session_round_trips_tool_exec_and_emits_done() {
        let scripted_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "exit_code": 0,
                "stdout": "hello from harbor",
                "stderr": "",
                "timed_out": false,
            }
        });
        let canned = format!("{scripted_response}\n");
        let reader = Cursor::new(canned.into_bytes());
        let mut writer = Vec::<u8>::new();
        {
            let mut session = HarborSession::new(reader, &mut writer);
            let result = run_stub_agent("rename foo to bar", &mut session)
                .expect("stub agent");
            assert_eq!(result.exit_code, 0);
            assert_eq!(result.stdout, "hello from harbor");
            assert!(!result.timed_out);
        }

        // What kimetsu wrote: line 1 should be tool.exec (id=1, echo),
        // line 2 should be agent.done (no id).
        let written = String::from_utf8(writer).expect("utf8");
        let mut lines = written.lines();

        let req: Value = serde_json::from_str(lines.next().expect("tool.exec line"))
            .expect("parse tool.exec");
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 1);
        assert_eq!(req["method"], "tool.exec");
        assert_eq!(req["params"]["program"], "echo");
        assert!(req["params"]["args"][0]
            .as_str()
            .unwrap()
            .contains("rename foo to bar"));

        let done: Value = serde_json::from_str(lines.next().expect("agent.done line"))
            .expect("parse agent.done");
        assert_eq!(done["jsonrpc"], "2.0");
        assert!(done["id"].is_null(), "agent.done is a notification");
        assert_eq!(done["method"], "agent.done");
        let summary = done["params"]["summary"].as_str().unwrap();
        assert!(summary.contains("rename foo to bar"));
        assert!(summary.contains(HARBOR_PROTOCOL_VERSION));

        assert!(lines.next().is_none(), "no trailing frames expected");
    }

    /// An id mismatch from the adapter must surface as an error rather
    /// than feeding stale data to the next stage.
    #[test]
    fn harbor_session_errors_on_id_mismatch() {
        let bogus = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "result": { "exit_code": 0, "stdout": "", "stderr": "", "timed_out": false }
        });
        let canned = format!("{bogus}\n");
        let reader = Cursor::new(canned.into_bytes());
        let mut writer = Vec::<u8>::new();
        let mut session = HarborSession::new(reader, &mut writer);
        let result = session.request_tool_exec(ToolExecParams {
            program: "echo".into(),
            args: vec!["hi".into()],
            cwd: None,
            timeout_secs: 10,
        });
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("id 99"), "got: {msg}");
    }

    /// A premature EOF (adapter dies before answering) must error
    /// cleanly so the bench shows a sensible failure mode.
    #[test]
    fn harbor_session_errors_on_premature_eof() {
        let reader = Cursor::new(Vec::<u8>::new());
        let mut writer = Vec::<u8>::new();
        let mut session = HarborSession::new(reader, &mut writer);
        let result = session.request_tool_exec(ToolExecParams {
            program: "echo".into(),
            args: vec![],
            cwd: None,
            timeout_secs: 10,
        });
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("closed stdin"), "got: {msg}");
    }

    /// JSON-RPC error frames must surface as errors with the adapter's
    /// message preserved.
    #[test]
    fn harbor_session_errors_on_jsonrpc_error_payload() {
        let err_frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32601, "message": "method not found in adapter" }
        });
        let canned = format!("{err_frame}\n");
        let reader = Cursor::new(canned.into_bytes());
        let mut writer = Vec::<u8>::new();
        let mut session = HarborSession::new(reader, &mut writer);
        let result = session.request_tool_exec(ToolExecParams {
            program: "echo".into(),
            args: vec![],
            cwd: None,
            timeout_secs: 10,
        });
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("method not found in adapter"), "got: {msg}");
        assert!(msg.contains("-32601"), "got: {msg}");
    }
}
