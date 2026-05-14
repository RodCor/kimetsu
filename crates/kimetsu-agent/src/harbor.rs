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

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use kimetsu_core::KimetsuResult;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tools::{
    CommandSpec, RawShellOutput, ShellExecutor, ToolRuntimeConfig,
};

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

/// MP-7c: `ShellExecutor` impl that proxies every `shell_command` call
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

/// MP-7c: multi-step stub that exercises the full `ShellExecutor` path.
/// Runs two shell commands through `HarborShellExecutor` (`pwd` to
/// surface the workspace root, then `echo` to confirm the task made it
/// across), then emits `agent.done`. Returns the captured outputs so
/// tests can assert on per-step behavior.
///
/// MP-7d will replace this with a real model loop that issues
/// `shell_command` calls based on the task description. The protocol
/// surface and tool routing stay identical.
pub fn run_multi_step_stub<R: BufRead + 'static, W: Write + 'static>(
    task: &str,
    session: Rc<RefCell<HarborSession<R, W>>>,
    runtime: &mut crate::tools::ToolRuntime,
) -> KimetsuResult<MultiStepStubReport> {
    let pwd_out = runtime.shell_command(CommandSpec {
        program: "pwd".into(),
        args: vec![],
        cwd_relative: None,
        timeout_secs: Some(15),
        expected_exit: Some(0),
    })?;
    let echo_out = runtime.shell_command(CommandSpec {
        program: "echo".into(),
        args: vec![format!("kimetsu MP-7c routed task: {task}")],
        cwd_relative: None,
        timeout_secs: Some(15),
        expected_exit: Some(0),
    })?;

    session.borrow_mut().emit_done(AgentDoneParams {
        summary: format!(
            "MP-7c multi-step stub for `{task}` completed; protocol={HARBOR_PROTOCOL_VERSION}"
        ),
        context: Some(json!({
            "stub": "multi-step",
            "steps": [
                { "program": "pwd",  "exit_code": pwd_out.exit_code },
                { "program": "echo", "exit_code": echo_out.exit_code },
            ],
        })),
    })?;

    Ok(MultiStepStubReport {
        pwd: pwd_out,
        echo: echo_out,
    })
}

#[derive(Debug)]
pub struct MultiStepStubReport {
    pub pwd: crate::tools::ShellCommandOutput,
    pub echo: crate::tools::ShellCommandOutput,
}

/// MP-7d: maximum turns of model ↔ tool ping-pong before we force the
/// agent to wrap up. 25 mirrors the loop budget used inside the v0.1
/// pipeline; we surface it here as a const so MP-8 can tune it from the
/// Terminal-Bench data.
pub const DEFAULT_MODEL_TURN_BUDGET: u32 = 25;

/// MP-7d: report returned by `run_model_agent`. Mostly for tests + the
/// CLI smoke surface; production runs care about the JSON-RPC frames
/// already emitted to Harbor.
#[derive(Debug)]
pub struct ModelAgentReport {
    pub turns: u32,
    pub tool_calls: u32,
    pub stop_reason: crate::model::StopReason,
    pub final_text: Option<String>,
    pub usage: crate::model::TokenUsage,
}

/// MP-7d: the real agent loop. Wires a `ModelProvider` (claude_code,
/// anthropic, or a `MockProvider` in tests) to a `ToolRuntime` whose
/// shell backend routes through Harbor. The model issues
/// `shell_command` calls based on the task; we run them via the runtime
/// (which proxies via HarborShellExecutor → JSON-RPC → Harbor →
/// container), feed the result back as a tool result message, and loop
/// until the model returns plain text or we exhaust the turn budget.
///
/// `agent.done` carries the model's final text as the summary so
/// Terminal-Bench's grader sees a real answer, not a stub string. The
/// session's frame stream is identical in shape to MP-7c — only the
/// content changes.
pub fn run_model_agent<R, W>(
    task: &str,
    session: Rc<RefCell<HarborSession<R, W>>>,
    runtime: &mut crate::tools::ToolRuntime,
    provider: &mut dyn crate::model::ModelProvider,
    turn_budget: u32,
    brain_context: Option<&str>,
) -> KimetsuResult<ModelAgentReport>
where
    R: BufRead + 'static,
    W: Write + 'static,
{
    use crate::model::{ModelMessage, ModelRequest, StopReason, ToolChoice, ToolDefinition};

    // MP-9 (path B): Claude Code 2.x in `-p` mode injects its own
    // agentic harness over our --system-prompt and tells the model it
    // has Monitor / PushNotification / RemoteTrigger tools. The MP-8
    // gauntlet caught the model trusting CC's harness over our system
    // prompt and never invoking shell_command.
    //
    // The fix is to make the envelope contract authoritative in the
    // USER message (which the model reads last, right before
    // responding). The system prompt is still descriptive; the user
    // message carries the actual task PLUS an explicit override note
    // that:
    //   1. tells the model to ignore any other tool catalog CC may have
    //      mentioned;
    //   2. repeats the envelope grammar verbatim;
    //   3. asks for a tool_call envelope as the first response.
    // This is the v0.1 envelope pattern adapted to compete with CC's
    // harness override.
    let system = MessageMessage::system_prompt_for_harbor();

    // MP-11 (brain mode): if a kimetsu project supplied broker context
    // for this task — curated memories, prior-run capsules — render it
    // as a "Prior context" section the model sees BEFORE the task.
    // This is the kimetsu-brain leg of the v0.2 falsifiable claim. In
    // no-brain mode `brain_context` is None and the rendered section is
    // omitted entirely (no "empty memories" stub that would dilute the
    // model's attention).
    let prior_block = match brain_context {
        Some(text) if !text.trim().is_empty() => format!(
            "=== Prior context (from Kimetsu's broker — curated memories \
             and prior-run capsules retrieved for this task) ===\n\
             {text}\n\n",
        ),
        _ => String::new(),
    };

    let user = ModelMessage::user_text(format!(
        "{prior_block}\
         Task (from Harbor / Terminal-Bench):\n\
         {task}\n\n\
         === Important runtime override ===\n\
         You are running inside the Kimetsu wrapper. Any tool catalog the\n\
         Claude Code runtime advertises (Monitor, PushNotification,\n\
         RemoteTrigger, Bash, Edit, etc.) is NOT real in this environment.\n\
         The only way to take an action is to emit a JSON envelope that\n\
         Kimetsu will parse out of your response text and execute on your\n\
         behalf. There is exactly one usable tool: `shell_command`.\n\
         \n\
         Response format (one JSON object per reply, no prose, no\n\
         markdown, no backticks):\n\
         \n\
         To run a command:\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"tool_call\": {{\"name\": \"shell_command\",\n\
                          \"input\": {{\"program\": \"<bin>\",\n\
                                     \"args\": [\"<arg1>\", \"<arg2>\"],\n\
                                     \"cwd_relative\": \"<dir or null>\",\n\
                                     \"timeout_secs\": 60}}}}}}\n\
         \n\
         To finish and report the final answer:\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"finish\": {{\"summary\": \"<one-line outcome the verifier should see>\"}}}}\n\
         \n\
         Workspace paths are relative to the task's starting directory.\n\
         Begin with one tool_call envelope. Do not narrate."
    ));
    let mut messages = vec![system, user];

    let tool_defs = vec![ToolDefinition {
        name: "shell_command".to_string(),
        description: "Run a single shell command inside the workspace. \
            Returns exit_code, stdout, stderr. Use this to read files \
            (cat), list dirs (ls), search (grep / rg), edit (sed, echo \
            > file), and run build / test commands."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "program": { "type": "string", "description": "Program name, e.g. `bash` or `cat`." },
                "args":    { "type": "array", "items": { "type": "string" }, "description": "Command arguments." },
                "cwd_relative": { "type": "string", "description": "Workspace-relative working directory; default is workspace root." },
                "timeout_secs": { "type": "integer", "description": "Hard timeout in seconds; default 60." }
            },
            "required": ["program"],
        }),
    }];

    let mut tool_calls_total = 0u32;
    let mut last_usage = crate::model::TokenUsage::default();
    let mut final_text: Option<String> = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut turn = 0u32;

    while turn < turn_budget {
        turn += 1;

        let request = ModelRequest {
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 4096,
            temperature: 0.0,
            metadata: serde_json::json!({
                "kimetsu_mode": "harbor",
                "turn": turn,
                "task_preview": preview_text(task, 120),
            }),
        };

        let response = provider.complete(request)?;
        last_usage = response.usage;
        stop_reason = response.stop_reason.clone();

        if !response.tool_calls.is_empty() {
            messages.push(ModelMessage::assistant_tool_calls(response.tool_calls.clone()));

            for call in response.tool_calls {
                tool_calls_total += 1;
                if call.name != "shell_command" {
                    let err = serde_json::json!({
                        "error": format!(
                            "unsupported tool `{}`; only `shell_command` is available in harbor mode",
                            call.name
                        ),
                    });
                    messages.push(ModelMessage::tool_result(call.id, call.name, err));
                    continue;
                }
                let spec_value = call.input;
                let result_value = match serde_json::from_value::<crate::tools::CommandSpec>(
                    spec_value.clone(),
                ) {
                    Ok(spec) => match runtime.shell_command(spec) {
                        Ok(output) => serde_json::to_value(output).unwrap_or_else(|err| {
                            serde_json::json!({ "error": err.to_string() })
                        }),
                        Err(err) => serde_json::json!({
                            "error": format!("shell_command failed: {err}"),
                        }),
                    },
                    Err(err) => serde_json::json!({
                        "error": format!("invalid shell_command input: {err}; got {spec_value}"),
                    }),
                };
                messages.push(ModelMessage::tool_result(call.id, "shell_command", result_value));
            }
            continue;
        }

        // No tool calls -> the model is done answering. Capture the text.
        if let Some(text) = response.text.clone() {
            final_text = Some(text.clone());
            messages.push(ModelMessage::assistant_text(text));
        } else {
            // Empty response with no tool call: stop, surface the
            // protocol-level stop reason so MP-8 telemetry can see what
            // went wrong (refusal? max_tokens? api error?).
            final_text = None;
        }
        break;
    }

    let summary = final_text.clone().unwrap_or_else(|| {
        format!(
            "kimetsu MP-7d agent ran {turn} turn(s) without producing a final answer (stop_reason={stop_reason:?})"
        )
    });

    session.borrow_mut().emit_done(AgentDoneParams {
        summary,
        context: Some(serde_json::json!({
            "mode": "model_agent",
            "turns": turn,
            "tool_calls": tool_calls_total,
            "stop_reason": format!("{stop_reason:?}"),
            "input_tokens": last_usage.input_tokens,
            "output_tokens": last_usage.output_tokens,
            "cost_usd": last_usage.cost_usd,
            "protocol_version": HARBOR_PROTOCOL_VERSION,
        })),
    })?;

    Ok(ModelAgentReport {
        turns: turn,
        tool_calls: tool_calls_total,
        stop_reason,
        final_text,
        usage: last_usage,
    })
}

fn preview_text(text: &str, limit: usize) -> String {
    let mut clipped: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        clipped.push('…');
    }
    clipped
}

/// MP-7d: namespace helper so the system prompt is one focused location
/// rather than scattered as `format!` calls throughout the loop. The
/// prompt is intentionally short and Terminal-Bench-oriented; MP-8 will
/// fold in the broker context (memories + prior-run capsules) here.
struct MessageMessage;
impl MessageMessage {
    /// MP-9 (path B): minimal role description for the harbor-mode
    /// agent. All format rules — envelope grammar, tool catalog, the
    /// override notice that Claude Code's internal harness tools
    /// (Monitor / PushNotification / RemoteTrigger / Bash / Edit) are
    /// NOT real here — live in the user message in `run_model_agent`,
    /// not here. The system prompt is whatever Claude Code's `-p`
    /// runtime allows; the user message is what the model reads last
    /// before responding, and that's where the authority needs to be.
    fn system_prompt_for_harbor() -> crate::model::ModelMessage {
        crate::model::ModelMessage {
            role: crate::model::MessageRole::System,
            content: vec![crate::model::MessageContent::Text {
                text: concat!(
                    "You are Kimetsu, a coding agent driving a sandboxed Linux ",
                    "shell inside Harbor / Terminal-Bench. Follow the response ",
                    "format described in the user message exactly. Be concise; ",
                    "no narration between actions."
                ).to_string(),
            }],
        }
    }
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

    /// MP-7c: drive the full multi-step routed flow through a
    /// HarborShellExecutor wired into a real ToolRuntime. The fake
    /// adapter returns scripted ToolExecResult values per request; we
    /// assert that:
    ///   - both shell_command calls came back routed (exit_code visible)
    ///   - the harness wrote two tool.exec frames in order, followed by
    ///     agent.done with the routed context summary
    #[test]
    fn multi_step_stub_routes_two_shell_commands_through_harbor() {
        use crate::tools::{ToolRuntime, ToolRuntimeConfig};
        use kimetsu_core::ids::RunId;
        use std::fs;

        // The two scripted responses, one per shell_command call.
        let pwd_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "exit_code": 0,
                "stdout": "/workspace",
                "stderr": "",
                "timed_out": false,
            }
        });
        let echo_response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "exit_code": 0,
                "stdout": "kimetsu MP-7c routed task: bench fixture",
                "stderr": "",
                "timed_out": false,
            }
        });
        let canned = format!("{pwd_response}\n{echo_response}\n");

        // Reader / writer for HarborSession.
        let reader = Cursor::new(canned.into_bytes());
        let writer = Vec::<u8>::new();
        let session = Rc::new(RefCell::new(HarborSession::new(reader, writer)));

        // ToolRuntime needs a real on-disk root for redaction artifact
        // bookkeeping (still local — only the subprocess execution
        // routes through Harbor). Use a temp dir.
        let root = std::env::temp_dir().join(format!("kimetsu-harbor-test-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create temp root");

        let report = {
            let executor = Box::new(HarborShellExecutor::new(Rc::clone(&session)));
            let mut runtime = ToolRuntime::new(&root, RunId::new())
                .expect("runtime")
                .with_shell_executor(executor)
                .with_config(ToolRuntimeConfig {
                    redact_secrets: false,
                    ..ToolRuntimeConfig::default()
                });
            let report = run_multi_step_stub("bench fixture", Rc::clone(&session), &mut runtime)
                .expect("multi-step stub");
            // Drop runtime here so the Rc<RefCell<...>> only has one
            // remaining strong reference for the asserts below.
            drop(runtime);
            report
        };

        assert_eq!(report.pwd.exit_code, 0);
        assert!(report.pwd.stdout_summary.contains("/workspace"));
        assert_eq!(report.echo.exit_code, 0);
        assert!(report.echo.stdout_summary.contains("bench fixture"));

        // Pull the writer back out and inspect the frames we sent.
        let session_inner = Rc::try_unwrap(session)
            .map_err(|_| "rc still has outstanding refs")
            .unwrap()
            .into_inner();
        let written = String::from_utf8(session_inner.writer).expect("utf8");
        let mut lines = written.lines();

        let req1: Value = serde_json::from_str(lines.next().expect("pwd line")).unwrap();
        assert_eq!(req1["method"], "tool.exec");
        assert_eq!(req1["id"], 1);
        assert_eq!(req1["params"]["program"], "pwd");

        let req2: Value = serde_json::from_str(lines.next().expect("echo line")).unwrap();
        assert_eq!(req2["method"], "tool.exec");
        assert_eq!(req2["id"], 2);
        assert_eq!(req2["params"]["program"], "echo");
        assert!(req2["params"]["args"][0]
            .as_str()
            .unwrap()
            .contains("bench fixture"));

        let done: Value = serde_json::from_str(lines.next().expect("done line")).unwrap();
        assert_eq!(done["method"], "agent.done");
        assert!(done["params"]["summary"]
            .as_str()
            .unwrap()
            .contains("bench fixture"));
        assert_eq!(done["params"]["context"]["stub"], "multi-step");
        let steps = done["params"]["context"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["program"], "pwd");
        assert_eq!(steps[1]["program"], "echo");

        assert!(lines.next().is_none(), "no trailing frames expected");

        fs::remove_dir_all(root).ok();
    }

    /// MP-7d: drive `run_model_agent` with a MockProvider that scripts
    /// three turns:
    ///   turn 1 - issue a shell_command tool call (`ls` on the workspace)
    ///   turn 2 - issue a second shell_command tool call (`cat README.md`)
    ///   turn 3 - return a plain text final answer
    /// The fake Harbor responds with canned ToolExecResults for each
    /// routed shell command. Assert:
    /// - exactly 2 tool.exec frames went to Harbor
    /// - the model saw both tool results before producing its final text
    /// - agent.done summary == the model's final text
    /// - ModelAgentReport carries turns=3, tool_calls=2, EndTurn
    #[test]
    fn model_agent_drives_tool_loop_and_emits_done_with_final_text() {
        use crate::model::{ModelResponse, MockProvider};
        use crate::tools::{ToolRuntime, ToolRuntimeConfig};
        use kimetsu_core::ids::RunId;
        use std::fs;

        // Two scripted ToolExecResult replies (one per shell command).
        let ls_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "exit_code": 0,
                "stdout": "README.md\nsrc/\n",
                "stderr": "",
                "timed_out": false,
            }
        });
        let cat_response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "exit_code": 0,
                "stdout": "# Project\nThis is a test repo.\n",
                "stderr": "",
                "timed_out": false,
            }
        });
        let canned = format!("{ls_response}\n{cat_response}\n");
        let reader = Cursor::new(canned.into_bytes());
        let writer = Vec::<u8>::new();
        let session = Rc::new(RefCell::new(HarborSession::new(reader, writer)));

        // Three scripted model responses.
        let final_answer = "The project is a test repo. README explains it.";
        let model_responses = vec![
            ModelResponse::tool_call(
                "call_1",
                "shell_command",
                json!({ "program": "ls", "args": [], "cwd_relative": null, "timeout_secs": 15 }),
            ),
            ModelResponse::tool_call(
                "call_2",
                "shell_command",
                json!({
                    "program": "cat",
                    "args": ["README.md"],
                    "cwd_relative": null,
                    "timeout_secs": 15,
                }),
            ),
            ModelResponse::text(final_answer),
        ];
        let mut provider = MockProvider::new(model_responses);

        // ToolRuntime with HarborShellExecutor on a scratch dir.
        let root = std::env::temp_dir().join(format!("kimetsu-mp7d-test-{}", RunId::new()));
        fs::create_dir_all(&root).expect("create scratch root");

        let report = {
            let executor = Box::new(HarborShellExecutor::new(Rc::clone(&session)));
            let mut runtime = ToolRuntime::new(&root, RunId::new())
                .expect("runtime")
                .with_shell_executor(executor)
                .with_config(ToolRuntimeConfig {
                    redact_secrets: false,
                    ..ToolRuntimeConfig::default()
                });
            let report = run_model_agent(
                "summarize the README",
                Rc::clone(&session),
                &mut runtime,
                &mut provider,
                DEFAULT_MODEL_TURN_BUDGET,
                None,
            )
            .expect("model agent");
            drop(runtime);
            report
        };

        assert_eq!(report.turns, 3);
        assert_eq!(report.tool_calls, 2);
        assert_eq!(report.final_text.as_deref(), Some(final_answer));

        // Drain the writer and inspect the JSON-RPC frame sequence:
        // tool.exec (id=1, ls) -> tool.exec (id=2, cat README.md) -> agent.done.
        let session_inner = Rc::try_unwrap(session)
            .map_err(|_| "rc still has outstanding refs")
            .unwrap()
            .into_inner();
        let written = String::from_utf8(session_inner.writer).expect("utf8");
        let mut lines = written.lines();

        let req1: Value = serde_json::from_str(lines.next().expect("ls line")).unwrap();
        assert_eq!(req1["method"], "tool.exec");
        assert_eq!(req1["id"], 1);
        assert_eq!(req1["params"]["program"], "ls");

        let req2: Value = serde_json::from_str(lines.next().expect("cat line")).unwrap();
        assert_eq!(req2["method"], "tool.exec");
        assert_eq!(req2["id"], 2);
        assert_eq!(req2["params"]["program"], "cat");
        assert_eq!(req2["params"]["args"][0], "README.md");

        let done: Value = serde_json::from_str(lines.next().expect("done line")).unwrap();
        assert_eq!(done["method"], "agent.done");
        assert_eq!(done["params"]["summary"].as_str().unwrap(), final_answer);
        assert_eq!(done["params"]["context"]["mode"], "model_agent");
        assert_eq!(done["params"]["context"]["turns"], 3);
        assert_eq!(done["params"]["context"]["tool_calls"], 2);

        assert!(lines.next().is_none(), "no trailing frames expected");
        fs::remove_dir_all(root).ok();

        // Confirm the model saw the right conversation shape: 1 system,
        // 1 user (task), then alternating assistant-tool-call / tool-result
        // pairs, then a final assistant-text turn.
        let observed_msgs = &provider.requests.last().expect("last request").messages;
        // System + user + 2*(tool_call + tool_result) = 6 messages on turn 3.
        assert!(
            observed_msgs.len() >= 6,
            "expected >= 6 messages by turn 3, got {}: {observed_msgs:?}",
            observed_msgs.len()
        );
        assert!(matches!(
            observed_msgs[0].role,
            crate::model::MessageRole::System
        ));
        assert!(matches!(
            observed_msgs[1].role,
            crate::model::MessageRole::User
        ));
    }

    /// MP-7d: turn-budget guard. A model that never returns plain text
    /// must be cut off after `turn_budget` iterations, with `agent.done`
    /// still emitted carrying the budget-exhaustion summary so Harbor
    /// records a clean termination instead of a hang.
    #[test]
    fn model_agent_stops_when_turn_budget_exhausted() {
        use crate::model::{ModelResponse, MockProvider};
        use crate::tools::{ToolRuntime, ToolRuntimeConfig};
        use kimetsu_core::ids::RunId;
        use std::fs;

        // Pre-queue 3 ToolExecResults to match the 3 turn budget.
        let mut canned = String::new();
        for id in 1..=3u64 {
            canned.push_str(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "exit_code": 0,
                    "stdout": format!("turn {id}"),
                    "stderr": "",
                    "timed_out": false,
                }
            }).to_string());
            canned.push('\n');
        }
        let reader = Cursor::new(canned.into_bytes());
        let writer = Vec::<u8>::new();
        let session = Rc::new(RefCell::new(HarborSession::new(reader, writer)));

        // Three tool-call responses, no final text -> budget exhausted.
        let model_responses: Vec<ModelResponse> = (1..=3)
            .map(|i| {
                ModelResponse::tool_call(
                    format!("call_{i}"),
                    "shell_command",
                    json!({ "program": "echo", "args": [format!("step {i}")] }),
                )
            })
            .collect();
        let mut provider = MockProvider::new(model_responses);

        let root = std::env::temp_dir().join(format!("kimetsu-mp7d-budget-{}", RunId::new()));
        fs::create_dir_all(&root).expect("scratch");

        let report = {
            let executor = Box::new(HarborShellExecutor::new(Rc::clone(&session)));
            let mut runtime = ToolRuntime::new(&root, RunId::new())
                .expect("runtime")
                .with_shell_executor(executor)
                .with_config(ToolRuntimeConfig {
                    redact_secrets: false,
                    ..ToolRuntimeConfig::default()
                });
            let report = run_model_agent(
                "loop forever",
                Rc::clone(&session),
                &mut runtime,
                &mut provider,
                3,
                None,
            )
            .expect("budget-capped agent");
            drop(runtime);
            report
        };

        assert_eq!(report.turns, 3, "should stop at budget");
        assert_eq!(report.tool_calls, 3, "all 3 tool calls fired");
        assert!(report.final_text.is_none(), "no final text produced");

        let session_inner = Rc::try_unwrap(session)
            .map_err(|_| "rc held")
            .unwrap()
            .into_inner();
        let written = String::from_utf8(session_inner.writer).expect("utf8");
        // 3 tool.exec frames + 1 agent.done = 4 frames
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 4, "expected 3 tool.exec + 1 agent.done");
        let done: Value = serde_json::from_str(lines[3]).unwrap();
        assert_eq!(done["method"], "agent.done");
        let summary = done["params"]["summary"].as_str().unwrap();
        assert!(
            summary.contains("3 turn") && summary.contains("without producing a final answer"),
            "budget-exhausted summary: {summary}"
        );
        fs::remove_dir_all(root).ok();
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
