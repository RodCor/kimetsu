//! Line-oriented JSON-RPC session for the Kimetsu â†” Harbor protocol.
//!
//! v0.3.2 â€” physically moved from `kimetsu-agent::harness`. The
//! session was always harbor-specific; living in kimetsu-agent forced
//! kimetsu-chat (and any other future transport) to compile JSON-RPC
//! plumbing it never uses. The agent loop is now transport-agnostic
//! (Phase-2 v0.3.1) â€” only this crate owns the wire types.

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kimetsu_agent::tools::{CommandSpec, RawShellOutput, ShellExecutor, ToolRuntimeConfig};
use kimetsu_core::KimetsuResult;
use serde_json::{Value, json};

use crate::protocol::{AgentDoneParams, JsonRpcResponse, ToolExecParams, ToolExecResult};

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
        let result_value = response
            .result
            .ok_or_else(|| "harbor adapter response missing both result and error".to_string())?;
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

/// `ShellExecutor` impl that proxies every `shell_command` call
/// through a shared `HarborSession`. The session lives in
/// `Rc<RefCell<...>>` so the agent function can hand a clone to the
/// executor (boxed and owned by `ToolRuntime`) while keeping its own
/// handle for the final `agent.done` emission.
pub struct HarborShellExecutor<R: BufRead, W: Write> {
    session: Rc<RefCell<HarborSession<R, W>>>,
}

impl<R: BufRead, W: Write> HarborShellExecutor<R, W> {
    pub fn new(session: Rc<RefCell<HarborSession<R, W>>>) -> Self {
        Self { session }
    }
}

impl<R: BufRead + 'static, W: Write + 'static> ShellExecutor for HarborShellExecutor<R, W> {
    fn execute(
        &mut self,
        _repo_root: &Path,
        spec: &CommandSpec,
        config: &ToolRuntimeConfig,
    ) -> KimetsuResult<RawShellOutput> {
        let started = Instant::now();
        let timeout_secs = spec
            .timeout_secs
            .unwrap_or(config.default_timeout_secs)
            .min(config.max_timeout_secs);
        let result = self
            .session
            .borrow_mut()
            .request_tool_exec(ToolExecParams {
                program: spec.program.clone(),
                args: spec.args.clone(),
                cwd: spec.cwd_relative.clone(),
                timeout_secs,
            })?;
        Ok(RawShellOutput {
            exit_code: result.exit_code,
            stdout: result.stdout.into_bytes(),
            stderr: result.stderr.into_bytes(),
            timed_out: result.timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::HARBOR_PROTOCOL_VERSION;
    use std::io::Cursor;

    /// Round-trip a single tool.exec: kimetsu writes the request,
    /// the harness "responds" with a canned ToolExecResult, kimetsu
    /// reads it back and surfaces it.
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
            let result = session
                .request_tool_exec(ToolExecParams {
                    program: "echo".into(),
                    args: vec!["hello".into()],
                    cwd: None,
                    timeout_secs: 5,
                })
                .expect("tool_exec");
            assert_eq!(result.exit_code, 0);
            assert_eq!(result.stdout, "hello from harbor");
            session
                .emit_done(AgentDoneParams {
                    summary: "done".into(),
                    context: Some(json!({"protocol": HARBOR_PROTOCOL_VERSION})),
                })
                .expect("emit_done");
        }
        let written = String::from_utf8(writer).expect("utf8 writer");
        let mut lines = written.lines();
        let req_line = lines.next().expect("request line");
        let req: Value = serde_json::from_str(req_line).expect("request json");
        assert_eq!(req["method"], "tool.exec");
        assert_eq!(req["id"], 1);
        let done_line = lines.next().expect("done line");
        let done: Value = serde_json::from_str(done_line).expect("done json");
        assert_eq!(done["method"], "agent.done");
        assert_eq!(done["params"]["summary"], "done");
    }

    #[test]
    fn harbor_session_errors_on_id_mismatch() {
        let bad = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "result": {"exit_code": 0, "stdout": "", "stderr": "", "timed_out": false}
        });
        let canned = format!("{bad}\n");
        let reader = Cursor::new(canned.into_bytes());
        let mut writer = Vec::<u8>::new();
        let mut session = HarborSession::new(reader, &mut writer);
        let result = session.request_tool_exec(ToolExecParams {
            program: "echo".into(),
            args: vec![],
            cwd: None,
            timeout_secs: 5,
        });
        let err = format!("{}", result.expect_err("expected id mismatch error"));
        assert!(err.contains("id 99"), "got: {err}");
    }

    #[test]
    fn harbor_session_errors_on_premature_eof() {
        let reader = Cursor::new(Vec::<u8>::new());
        let mut writer = Vec::<u8>::new();
        let mut session = HarborSession::new(reader, &mut writer);
        let result = session.request_tool_exec(ToolExecParams {
            program: "echo".into(),
            args: vec![],
            cwd: None,
            timeout_secs: 5,
        });
        let err = format!("{}", result.expect_err("expected premature EOF"));
        assert!(err.contains("closed stdin"), "got: {err}");
    }

    #[test]
    fn harbor_session_errors_on_jsonrpc_error_payload() {
        let scripted = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "method not found in adapter"}
        });
        let canned = format!("{scripted}\n");
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
