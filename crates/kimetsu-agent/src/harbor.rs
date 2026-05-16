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
// MP-13b: bumped 25 -> 40. MP-12 trial logs showed compile-compcert,
// caffe-cifar-10, install-windows-3-11 hitting the budget cap before
// finishing. 40 turns gives ~60% more headroom while still bounding
// runaway cost.
pub const DEFAULT_MODEL_TURN_BUDGET: u32 = 40;

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
/// MP-13: opts for tuning the harbor agent loop. Defaults match
/// production CLI usage. Tests use `HarborAgentOpts::for_tests()` to
/// disable the auto-orient pre-shell + the persistence gate so
/// scripted MockProvider responses aren't perturbed.
#[derive(Debug, Clone, Copy)]
pub struct HarborAgentOpts {
    pub turn_budget: u32,
    pub auto_orient: bool,
    pub min_actions_before_finish: u32,
}

impl Default for HarborAgentOpts {
    fn default() -> Self {
        Self {
            turn_budget: DEFAULT_MODEL_TURN_BUDGET,
            auto_orient: true,
            min_actions_before_finish: 3,
        }
    }
}

impl HarborAgentOpts {
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            turn_budget: DEFAULT_MODEL_TURN_BUDGET,
            auto_orient: false,
            min_actions_before_finish: 0,
        }
    }
}

pub fn run_model_agent<R, W>(
    task: &str,
    session: Rc<RefCell<HarborSession<R, W>>>,
    runtime: &mut crate::tools::ToolRuntime,
    provider: &mut dyn crate::model::ModelProvider,
    opts: HarborAgentOpts,
    brain_context: Option<&str>,
) -> KimetsuResult<ModelAgentReport>
where
    R: BufRead + 'static,
    W: Write + 'static,
{
    let turn_budget = opts.turn_budget;
    let auto_orient = opts.auto_orient;
    let min_actions_before_finish = opts.min_actions_before_finish;
    use crate::model::{ModelMessage, ModelRequest, StopReason, ToolChoice};

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

    // MP-13a (auto-orient): bare Claude Code's agentic harness orients
    // the model implicitly — directory listing, README peek, build
    // system sniff — before the first model turn. The kimetsu wrapper
    // doesn't do that, so the model spends its first 3-5 turns on
    // `pwd && ls && cat README*` orientation. On a 25-40 turn budget
    // that's a ~15-20% tax on every task.
    //
    // Fix: run one composite shell command up front and inject the
    // result into the user message as "Initial workspace state". The
    // model arrives at the task description already knowing pwd,
    // top-level layout, and the likely task-instructions file. Saves
    // 1-3 turns per task and gives the model better grounding for its
    // first tool call.
    let orient_block = if auto_orient {
        match collect_workspace_orientation(runtime) {
            Some(text) if !text.trim().is_empty() => format!(
                "=== Initial workspace state (Kimetsu auto-orientation) ===\n\
                 {text}\n\n",
            ),
            _ => String::new(),
        }
    } else {
        String::new()
    };

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

    // MP-12: full v0.1-comparable tool surface (7 tools instead of just
    // shell_command). Each composed-tool implementation in harbor_tools.rs
    // dispatches one or more shell calls through the same
    // HarborShellExecutor that shell_command uses, then assembles a
    // structured result the model can rely on (read_file gets line
    // numbers + truncation; list_files gets a sized listing; etc.).
    // The model sees these as first-class JSON tools, which (a) cuts
    // the "I need to remember the exact bash invocation" overhead and
    // (b) collapses multi-step idioms (read-then-modify) into single
    // turns.
    let user = ModelMessage::user_text(format!(
        "{orient_block}\
         {prior_block}\
         Task (from Harbor / Terminal-Bench):\n\
         {task}\n\n\
         === Important runtime override ===\n\
         You are running inside the Kimetsu wrapper. Any tool catalog the\n\
         Claude Code runtime advertises (Monitor, PushNotification,\n\
         RemoteTrigger, Bash, Edit, etc.) is NOT real here.\n\
         The only way to take action is to emit a JSON envelope that\n\
         Kimetsu will parse out of your response text and execute. The\n\
         tool set below is what's available; ignore everything else.\n\
         \n\
         === Tools (pick the most specific one; fall back to shell_command) ===\n\
         - read_file:    {{path, max_lines?}} -> {{content, lines, truncated}}\n\
         - list_files:   {{path?, max_depth?, max_entries?}} -> {{entries}}\n\
         - search_files: {{pattern, path?, max_matches?, glob?}} -> {{matches}}\n\
         - edit_file:    {{path, old_string, new_string, replace_all?}} or {{path, edits:[...]}} -> {{bytes_delta}}\n\
         - write_file:   {{path, content}} -> {{path, bytes_written}}  (use ONLY for new files / full rewrites)\n\
         - apply_patch:  {{diff, cwd?, strip?}} -> {{files_patched, hunks_failed}}  (unified diff across files)\n\
         - git_status:   {{cwd?}} -> {{porcelain}}\n\
         - git_diff:     {{paths?, cwd?}} -> {{diff}}\n\
         - shell_command:{{program, args, cwd_relative?, timeout_secs?}} -> {{exit_code, stdout, stderr}}\n\
         \n\
         Response format (one JSON object per reply, no prose, no\n\
         markdown, no backticks):\n\
         \n\
         To call a single tool:\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"tool_call\": {{\"name\": \"<tool>\", \"input\": <object matching the schema>}}}}\n\
         \n\
         To batch several independent tools in one turn (saves model\n\
         round-trips when calls don't depend on each other's output):\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"tool_calls\": [\n\
            {{\"name\": \"<tool>\", \"input\": <object>}},\n\
            {{\"name\": \"<tool>\", \"input\": <object>}}\n\
          ]}}\n\
         Each call is dispatched and its result returned before the next\n\
         model turn. Only batch when the inputs are independent — if call\n\
         B needs to see B's output first, use the single form.\n\
         \n\
         To finish and report the final answer:\n\
         {{\"thought\": \"<short rationale>\",\n\
          \"finish\": {{\"summary\": \"<one-line outcome the verifier should see>\"}}}}\n\
         \n\
         Workspace paths are relative to the task's starting directory.\n\
         Use read_file / list_files / search_files for inspection;\n\
         edit_file for targeted in-place edits (cheaper than write_file\n\
         for small changes); apply_patch for multi-file unified-diff\n\
         changes; write_file only for new files or full rewrites;\n\
         shell_command for everything else (builds, tests, package\n\
         installs, invoking the verifier). Begin with one tool_call\n\
         envelope. Do not narrate."
    ));
    let mut messages = vec![system, user];

    let tool_defs = harbor_tool_definitions();

    let mut tool_calls_total = 0u32;
    let mut last_usage = crate::model::TokenUsage::default();
    let mut final_text: Option<String> = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut turn = 0u32;
    // MP-13d: flag so the persistence gate fires at most once per task.
    let mut persistence_nudge_sent = false;

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
                let name = call.name.clone();
                let result_value = harbor_dispatch_tool(runtime, &name, call.input.clone());
                messages.push(ModelMessage::tool_result(call.id, name, result_value));
            }
            continue;
        }

        // No tool calls -> the model is trying to finish.
        //
        // MP-13d (persistence gate): on real Terminal-Bench tasks we
        // observed the model emitting `finish` after 0-1 tool calls on
        // hard tasks like make-mips-interpreter — basically giving up
        // before even trying. Per the v0.2 plan's "kimetsu wraps the
        // model with a persistent harness" promise, we reject premature
        // finishes ONCE and push the model to actually try something.
        // Second time around we let the finish through so we don't loop
        // forever on a model that's genuinely stuck.
        if tool_calls_total < min_actions_before_finish && !persistence_nudge_sent {
            persistence_nudge_sent = true;
            let nudge = format!(
                "You have only taken {tool_calls_total} action(s) on this task. \
                 Terminal-Bench rarely accepts answers without inspection — \
                 at minimum read the relevant files, run the verifier or \
                 reproduce the task setup before declaring done. Please \
                 keep working: emit another tool_call envelope. If you \
                 genuinely believe the task requires no shell action, \
                 explain briefly in `thought` and then emit finish on \
                 your next turn."
            );
            if let Some(text) = response.text.clone() {
                messages.push(ModelMessage::assistant_text(text));
            }
            messages.push(ModelMessage::user_text(nudge));
            continue;
        }

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

// =====================================================================
// MP-12: composed-tool surface for harbor mode.
//
// The bare Claude-Code agent in MP-10b had 18.75 pp accuracy advantage
// over the kimetsu wrapper. The MP-11-RESULTS.md verdict traced it to
// a tool-surface gap: bare CC exposes Bash + Edit + Read + Glob + Grep
// + Write etc.; kimetsu exposed only `shell_command`. Common operations
// (read file -> edit -> verify) required 3-4 shell turns instead of one
// structured tool call.
//
// MP-12 closes the gap WITHOUT regressing the harbor protocol — each
// new tool is a thin Rust shim that composes 1-2 shell calls through
// the existing HarborShellExecutor and returns a structured JSON
// result. The model sees seven first-class tools; under the hood every
// tool eventually dispatches a `tool.exec` JSON-RPC frame to Harbor's
// container.
// =====================================================================

use crate::model::ToolDefinition;

const READ_FILE_DEFAULT_MAX_LINES: u32 = 800;
const LIST_FILES_DEFAULT_MAX_DEPTH: u32 = 3;
const LIST_FILES_DEFAULT_MAX_ENTRIES: u32 = 200;
const SEARCH_FILES_DEFAULT_MAX_MATCHES: u32 = 100;
const TOOL_DEFAULT_TIMEOUT_SECS: u64 = 60;

/// MP-12: the seven tools the harbor model sees. Order matters for
/// the model's first scan; put the most-used ones first (`read_file`,
/// `list_files`, `search_files`) so the catalog is read top-down.
pub fn harbor_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file from the workspace. Returns content \
                with line numbers, total line count, and a `truncated` flag if the \
                file exceeded `max_lines`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "max_lines": { "type": "integer", "description": "Cap on lines returned; default 800." }
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "list_files".to_string(),
            description: "List files under `path` up to `max_depth` directories deep. \
                Returns up to `max_entries` paths sorted alphabetically."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative root; default '.'." },
                    "max_depth": { "type": "integer", "description": "Default 3." },
                    "max_entries": { "type": "integer", "description": "Default 200." }
                },
            }),
        },
        ToolDefinition {
            name: "search_files".to_string(),
            description: "Grep for `pattern` (regex) under `path`. Returns up to \
                `max_matches` hits as {file, line, text}."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Default '.'." },
                    "max_matches": { "type": "integer", "description": "Default 100." },
                    "glob": { "type": "string", "description": "Optional filename glob, e.g. '*.py'." }
                },
                "required": ["pattern"],
            }),
        },
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Create or overwrite a file at `path` with `content`. \
                Returns `bytes_written`. Use this for NEW files or FULL rewrites; \
                for changing a few lines in an existing file use edit_file instead \
                (cheaper, less likely to corrupt unrelated parts)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
            }),
        },
        ToolDefinition {
            name: "edit_file".to_string(),
            description: "In-place replace `old_string` -> `new_string` in `path`. \
                `old_string` must occur exactly once unless `replace_all: true`. \
                Use this whenever you want to change a few lines in an existing \
                file — much cheaper than write_file's full rewrite. For multiple \
                edits in one call, pass `edits: [{old_string, new_string, replace_all?}, ...]` \
                instead of the single old_string/new_string pair."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Single-edit form: exact bytes to replace." },
                    "new_string": { "type": "string", "description": "Single-edit form: replacement bytes." },
                    "replace_all": { "type": "boolean", "description": "If old_string occurs more than once, replace every match. Default false." },
                    "edits": {
                        "type": "array",
                        "description": "Multi-edit form: array of {old_string, new_string, replace_all?}. Applied in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["old_string", "new_string"],
                        }
                    }
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "apply_patch".to_string(),
            description: "Apply a unified diff to one or more files. Use this when \
                making related changes across multiple files in one operation. \
                `strip` defaults to 0 (workspace-relative paths); set to 1 if your \
                diff has `a/` / `b/` prefixes. Returns the list of files patched \
                and any hunk failures."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "diff": { "type": "string", "description": "Unified-diff text (--- a/old\\n+++ b/new\\n@@ ...)" },
                    "cwd": { "type": "string", "description": "Workspace-relative directory to apply the patch in; default workspace root." },
                    "strip": { "type": "integer", "description": "patch -p<N> prefix-strip. Default 0." }
                },
                "required": ["diff"],
            }),
        },
        ToolDefinition {
            name: "git_status".to_string(),
            description: "Run `git status --porcelain` in `cwd`. Returns the raw \
                porcelain output."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Default workspace root." }
                },
            }),
        },
        ToolDefinition {
            name: "git_diff".to_string(),
            description: "Run `git diff` (working tree vs HEAD) for the given paths. \
                If `paths` is omitted, diffs everything."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string" }
                },
            }),
        },
        ToolDefinition {
            name: "shell_command".to_string(),
            description: "Escape hatch for anything the named tools don't cover: \
                builds, tests, package installs, running the verifier. \
                Returns exit_code, stdout, stderr."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd_relative": { "type": "string" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["program"],
            }),
        },
    ]
}

/// MP-12: route a model-emitted tool call to its composed-shell impl.
/// Returns a structured JSON value the agent loop hands back as a
/// tool_result message. Unknown tool names get an `error` object so
/// the model can self-correct without aborting the run.
pub fn harbor_dispatch_tool(
    runtime: &mut crate::tools::ToolRuntime,
    name: &str,
    input: Value,
) -> Value {
    match name {
        "read_file" => harbor_read_file(runtime, &input),
        "list_files" => harbor_list_files(runtime, &input),
        "search_files" => harbor_search_files(runtime, &input),
        "write_file" => harbor_write_file(runtime, &input),
        "edit_file" => harbor_edit_file(runtime, &input),
        "apply_patch" => harbor_apply_patch(runtime, &input),
        "git_status" => harbor_git_status(runtime, &input),
        "git_diff" => harbor_git_diff(runtime, &input),
        "shell_command" => harbor_shell_command(runtime, &input),
        other => json!({
            "error": format!(
                "unsupported tool `{other}`; pick one of \
                 read_file / list_files / search_files / write_file / \
                 edit_file / apply_patch / git_status / git_diff / shell_command"
            ),
        }),
    }
}

fn input_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}
fn input_u32(input: &Value, key: &str, default: u32) -> u32 {
    input
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| n as u32)
        .unwrap_or(default)
}

fn run_shell(
    runtime: &mut crate::tools::ToolRuntime,
    program: &str,
    args: Vec<String>,
    cwd_relative: Option<String>,
    timeout_secs: Option<u64>,
) -> Result<crate::tools::ShellCommandOutput, String> {
    let spec = CommandSpec {
        program: program.to_string(),
        args,
        cwd_relative,
        timeout_secs: Some(timeout_secs.unwrap_or(TOOL_DEFAULT_TIMEOUT_SECS)),
        expected_exit: None,
    };
    runtime.shell_command(spec).map_err(|e| e.to_string())
}

fn harbor_read_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "read_file requires `path`" });
    };
    let max_lines = input_u32(input, "max_lines", READ_FILE_DEFAULT_MAX_LINES);
    // `wc -l` first so we know how truncated we are; then sed -n to cap.
    // Both run in one shell so the model only sees one tool turn.
    let cmd = format!(
        "set -e; total=$(wc -l < {0} 2>/dev/null || echo 0); sed -n '1,{1}p' {0}; echo \"::LINES_TOTAL::$total\"",
        shell_quote(path),
        max_lines
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("read_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("read_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "path": path,
        });
    }
    // Split off the trailing ::LINES_TOTAL::N marker.
    let raw = out.stdout_summary;
    let (content, total_line_count) = split_total_marker(&raw);
    let returned_lines = content.lines().count() as u32;
    let truncated = total_line_count.map(|t| t > returned_lines).unwrap_or(false);
    json!({
        "path": path,
        "content": content,
        "lines_returned": returned_lines,
        "lines_total": total_line_count,
        "truncated": truncated,
        "duration_ms": out.duration_ms,
    })
}

fn harbor_list_files(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let path = input_str(input, "path").unwrap_or(".");
    let max_depth = input_u32(input, "max_depth", LIST_FILES_DEFAULT_MAX_DEPTH);
    let max_entries = input_u32(input, "max_entries", LIST_FILES_DEFAULT_MAX_ENTRIES);
    let args = vec![
        path.to_string(),
        "-maxdepth".into(),
        max_depth.to_string(),
        "-mindepth".into(),
        "1".into(),
        "-type".into(),
        "f".into(),
    ];
    let out = match run_shell(runtime, "find", args, None, Some(TOOL_DEFAULT_TIMEOUT_SECS)) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("list_files shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("list_files: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "path": path,
        });
    }
    let mut entries: Vec<&str> = out
        .stdout_summary
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    entries.sort();
    let total = entries.len() as u32;
    let truncated = total > max_entries;
    entries.truncate(max_entries as usize);
    json!({
        "path": path,
        "max_depth": max_depth,
        "entries": entries,
        "entries_returned": entries.len() as u32,
        "entries_total": total,
        "truncated": truncated,
    })
}

fn harbor_search_files(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(pattern) = input_str(input, "pattern") else {
        return json!({ "error": "search_files requires `pattern`" });
    };
    let path = input_str(input, "path").unwrap_or(".");
    let max_matches = input_u32(input, "max_matches", SEARCH_FILES_DEFAULT_MAX_MATCHES);
    let glob = input_str(input, "glob");

    // Use grep -rnE; --include for globs; head -n for cap. -I skips binary.
    let mut cmd = String::from("grep -rnEI ");
    if let Some(g) = glob {
        cmd.push_str(&format!("--include={} ", shell_quote(g)));
    }
    cmd.push_str(&shell_quote(pattern));
    cmd.push(' ');
    cmd.push_str(&shell_quote(path));
    cmd.push_str(&format!(" 2>/dev/null | head -n {max_matches} || true"));

    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("search_files shell failed: {e}") }),
    };
    let matches: Vec<Value> = out
        .stdout_summary
        .lines()
        .filter_map(parse_grep_line)
        .collect();
    json!({
        "pattern": pattern,
        "path": path,
        "matches": matches,
        "matches_returned": matches.len() as u32,
        "truncated": matches.len() as u32 >= max_matches,
    })
}

fn harbor_write_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "write_file requires `path`" });
    };
    let Some(content) = input_str(input, "content") else {
        return json!({ "error": "write_file requires `content`" });
    };
    // Heredoc with a fixed sentinel; base64 the content so any content
    // is safe (no need to escape backticks, $, etc.).
    let encoded = base64_encode(content.as_bytes());
    let cmd = format!(
        "set -e; mkdir -p \"$(dirname -- {0})\"; echo {1} | base64 -d > {0}; wc -c < {0}",
        shell_quote(path),
        shell_quote(&encoded)
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("write_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("write_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "path": path,
        });
    }
    let bytes_written: u64 = out.stdout_summary.trim().parse().unwrap_or(0);
    json!({
        "path": path,
        "bytes_written": bytes_written,
        "duration_ms": out.duration_ms,
    })
}

/// MP-14a: in-place replace `old_string` → `new_string` in `path`.
///
/// CC's Edit semantics: `old_string` must occur exactly once in the
/// file unless `replace_all = true`. The file is read, transformed
/// in Rust (avoiding shell quoting issues for arbitrary content),
/// and written back via the same base64 heredoc path as write_file
/// (so binaries / weird quotes stay safe).
///
/// Cheaper than write_file when the model only wants to change a
/// few lines: the model sends two short strings instead of the whole
/// file content, and the wire-format saves output tokens
/// proportional to file size.
fn harbor_edit_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "edit_file requires `path`" });
    };
    // Single-edit shape: {path, old_string, new_string, replace_all?}.
    // Multi-edit shape: {path, edits: [{old_string, new_string, replace_all?}, ...]}.
    let edits: Vec<EditOp> = if let Some(arr) = input.get("edits").and_then(Value::as_array) {
        let mut ops = Vec::with_capacity(arr.len());
        for (idx, e) in arr.iter().enumerate() {
            let Some(old) = e.get("old_string").and_then(Value::as_str) else {
                return json!({ "error": format!("edit_file: edits[{idx}].old_string required") });
            };
            let Some(new) = e.get("new_string").and_then(Value::as_str) else {
                return json!({ "error": format!("edit_file: edits[{idx}].new_string required") });
            };
            let replace_all = e.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
            ops.push(EditOp {
                old: old.to_string(),
                new: new.to_string(),
                replace_all,
            });
        }
        ops
    } else {
        let Some(old) = input_str(input, "old_string") else {
            return json!({ "error": "edit_file requires `old_string` or `edits` array" });
        };
        let Some(new) = input_str(input, "new_string") else {
            return json!({ "error": "edit_file requires `new_string` or `edits` array" });
        };
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        vec![EditOp {
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        }]
    };

    // Read the file via `cat | base64` so we can round-trip arbitrary
    // content (binary-safe). The base64 wrapper means the model's
    // edit strings are exact-bytes against the file content the user
    // sees from `cat`.
    let read_cmd = format!(
        "set -e; if [ ! -f {0} ]; then echo '__KIMETSU_MISSING__'; exit 0; fi; base64 -w 0 {0}",
        shell_quote(path)
    );
    let read_out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), read_cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("edit_file read shell failed: {e}") }),
    };
    if read_out.exit_code != 0 {
        return json!({
            "error": format!("edit_file: read failed exit {}; stderr: {}", read_out.exit_code, truncate(&read_out.stderr_summary, 400)),
            "path": path,
        });
    }
    let stdout = read_out.stdout_summary.trim();
    if stdout == "__KIMETSU_MISSING__" {
        return json!({
            "error": format!("edit_file: file not found at `{path}`. To create a new file, use write_file."),
            "path": path,
        });
    }
    let original_bytes = match base64_decode(stdout) {
        Ok(b) => b,
        Err(e) => return json!({ "error": format!("edit_file: base64 decode failed: {e}") }),
    };
    // We assume UTF-8 for edits; binary files should be left alone.
    let original = match std::str::from_utf8(&original_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            return json!({
                "error": format!("edit_file: `{path}` is not valid UTF-8; can't safely apply string replacements. Use write_file for full rewrites."),
                "path": path,
            });
        }
    };

    let mut content = original.clone();
    let mut applied = Vec::with_capacity(edits.len());
    for (idx, op) in edits.iter().enumerate() {
        if op.old.is_empty() {
            return json!({
                "error": format!("edit_file: edits[{idx}].old_string is empty"),
            });
        }
        if op.old == op.new {
            return json!({
                "error": format!("edit_file: edits[{idx}] old_string == new_string (no-op)"),
            });
        }
        let count = content.matches(&op.old).count();
        if count == 0 {
            return json!({
                "error": format!(
                    "edit_file: edits[{idx}].old_string not found in `{path}`. Read the file first to confirm the exact text (including whitespace / line endings)."
                ),
                "path": path,
            });
        }
        if count > 1 && !op.replace_all {
            return json!({
                "error": format!(
                    "edit_file: edits[{idx}].old_string occurs {count} times in `{path}`. Add `replace_all: true` to replace every occurrence, or include surrounding context to make the match unique."
                ),
                "path": path,
                "occurrences": count,
            });
        }
        if op.replace_all {
            content = content.replace(&op.old, &op.new);
        } else {
            // Replace exactly once.
            content = content.replacen(&op.old, &op.new, 1);
        }
        applied.push(json!({
            "edit_index": idx,
            "occurrences_replaced": if op.replace_all { count } else { 1 },
        }));
    }

    // Write the new content back through the same base64 heredoc as
    // write_file. This keeps binary-safety + permission inheritance
    // identical to a fresh write_file.
    let encoded = base64_encode(content.as_bytes());
    let write_cmd = format!(
        "set -e; mkdir -p \"$(dirname -- {0})\"; echo {1} | base64 -d > {0}; wc -c < {0}",
        shell_quote(path),
        shell_quote(&encoded)
    );
    let write_out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), write_cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("edit_file write shell failed: {e}") }),
    };
    if write_out.exit_code != 0 {
        return json!({
            "error": format!("edit_file: write failed exit {}; stderr: {}", write_out.exit_code, truncate(&write_out.stderr_summary, 400)),
            "path": path,
        });
    }
    let bytes_written: u64 = write_out.stdout_summary.trim().parse().unwrap_or(0);
    json!({
        "path": path,
        "edits_applied": applied,
        "bytes_before": original.len() as u64,
        "bytes_after": bytes_written,
        "bytes_delta": bytes_written as i64 - original.len() as i64,
    })
}

struct EditOp {
    old: String,
    new: String,
    replace_all: bool,
}

/// MP-14b: apply a unified diff to one or more files via container
/// `patch -p<strip>`. Codex's signature operation — lets the model
/// accumulate multi-file edits in one envelope instead of one
/// edit_file call per file.
///
/// Implementation:
///   1. base64-encode the diff (so quoting is safe).
///   2. pipe `echo $b64 | base64 -d | patch -p<strip> --batch -d <cwd>`.
///   3. parse stdout for "patching file X" lines to report which
///      files actually changed.
///
/// `strip` defaults to 0 (paths in the diff are workspace-relative).
/// Use 1 if your diff has a `a/` / `b/` prefix.
fn harbor_apply_patch(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(diff) = input_str(input, "diff") else {
        return json!({ "error": "apply_patch requires `diff` (unified-diff text)" });
    };
    let cwd = input_str(input, "cwd").map(str::to_string);
    let strip = input_u32(input, "strip", 0);

    let encoded = base64_encode(diff.as_bytes());
    let cmd = format!(
        "echo {} | base64 -d | patch -p{} --batch 2>&1",
        shell_quote(&encoded),
        strip
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        cwd.clone(),
        Some(TOOL_DEFAULT_TIMEOUT_SECS * 2), // patches can touch many files
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("apply_patch shell failed: {e}") }),
    };

    let mut patched_files: Vec<String> = Vec::new();
    let mut hunks_failed = 0u32;
    let mut rejects: Vec<String> = Vec::new();
    for line in out.stdout_summary.lines() {
        // GNU patch output: "patching file X" / "patching file X (Y hunk(s) succeeded ...)" / "X.rej created"
        if let Some(rest) = line.strip_prefix("patching file ") {
            // Strip optional trailing " (...)" info.
            let file = rest.split_once(' ').map(|(f, _)| f).unwrap_or(rest);
            patched_files.push(file.trim().to_string());
        } else if line.contains("FAILED") {
            hunks_failed += 1;
        } else if let Some(rest) = line.strip_prefix("Hunk") {
            if rest.contains("FAILED") {
                hunks_failed += 1;
            }
        } else if line.contains(".rej") {
            rejects.push(line.to_string());
        }
    }

    if out.exit_code != 0 {
        return json!({
            "error": format!("apply_patch: patch exited {}; output: {}", out.exit_code, truncate(&out.stdout_summary, 800)),
            "files_attempted": patched_files,
            "hunks_failed": hunks_failed,
            "rejects": rejects,
            "exit_code": out.exit_code,
        });
    }

    json!({
        "ok": true,
        "files_patched": patched_files,
        "hunks_failed": hunks_failed,
        "rejects": rejects,
        "output_excerpt": truncate(&out.stdout_summary, 800),
    })
}

fn harbor_git_status(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let cwd = input_str(input, "cwd").map(str::to_string);
    let out = match run_shell(
        runtime,
        "git",
        vec!["status".into(), "--porcelain".into()],
        cwd,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("git_status shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("git_status: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
        });
    }
    json!({
        "porcelain": out.stdout_summary,
        "clean": out.stdout_summary.trim().is_empty(),
    })
}

fn harbor_git_diff(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let cwd = input_str(input, "cwd").map(str::to_string);
    let mut args = vec!["diff".to_string()];
    if let Some(arr) = input.get("paths").and_then(Value::as_array) {
        args.push("--".to_string());
        for p in arr {
            if let Some(s) = p.as_str() {
                args.push(s.to_string());
            }
        }
    }
    let out = match run_shell(runtime, "git", args, cwd, Some(TOOL_DEFAULT_TIMEOUT_SECS)) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("git_diff shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("git_diff: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
        });
    }
    json!({
        "diff": out.stdout_summary,
        "empty": out.stdout_summary.trim().is_empty(),
    })
}

fn harbor_shell_command(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let spec_value = input.clone();
    match serde_json::from_value::<CommandSpec>(spec_value.clone()) {
        Ok(spec) => match runtime.shell_command(spec) {
            Ok(output) => serde_json::to_value(output)
                .unwrap_or_else(|err| json!({ "error": err.to_string() })),
            Err(err) => json!({ "error": format!("shell_command failed: {err}") }),
        },
        Err(err) => json!({
            "error": format!("invalid shell_command input: {err}; got {spec_value}"),
        }),
    }
}

// --- small helpers ---------------------------------------------------------

/// MP-13a: orient the model with one upfront shell command. Returns
/// a multi-block string (pwd / top-level ls / nearby task-instruction
/// files / build system sniff). Best-effort — on failure we return
/// None and the user message just omits the section, which is no
/// worse than the pre-MP-13 behavior.
///
/// We deliberately do this in a SINGLE composite shell call so it
/// costs one round-trip through the HarborSession instead of four.
fn collect_workspace_orientation(runtime: &mut crate::tools::ToolRuntime) -> Option<String> {
    let script = concat!(
        "echo '## pwd';",
        " pwd;",
        " echo '## top-level (ls -la)';",
        " ls -la 2>/dev/null | head -40;",
        " if [ -d /app ] && [ \"$(pwd)\" != /app ]; then",
        "   echo '## /app (ls -la)';",
        "   ls -la /app 2>/dev/null | head -40;",
        " fi;",
        " echo '## task-instruction files (head -80 of first match)';",
        " for f in TASK.md task.md INSTRUCTIONS.md INSTRUCTIONS.txt README.md README.txt README PROBLEM.md problem.md PROMPT.md prompt.md;",
        " do",
        "   if [ -r \"$f\" ]; then echo \"--- $f ---\"; head -80 \"$f\"; break; fi;",
        "   if [ -r \"/app/$f\" ]; then echo \"--- /app/$f ---\"; head -80 \"/app/$f\"; break; fi;",
        " done;",
        " echo '## build-system sniff';",
        " ls -1 Makefile makefile CMakeLists.txt setup.py pyproject.toml package.json Cargo.toml go.mod build.gradle pom.xml /app/Makefile /app/CMakeLists.txt /app/setup.py /app/pyproject.toml /app/package.json /app/Cargo.toml 2>/dev/null | head -10;",
        " echo '## done'",
    );

    let spec = crate::tools::CommandSpec {
        program: "bash".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        cwd_relative: None,
        timeout_secs: Some(30),
        expected_exit: None,
    };
    match runtime.shell_command(spec) {
        Ok(out) if out.exit_code == 0 || !out.stdout_summary.trim().is_empty() => {
            // Cap at ~3500 bytes so a runaway ls -la doesn't dominate
            // the user-message budget. Most task-instruction files
            // fit in 2-3 KB; ~3500 bytes still leaves headroom for
            // the task description and the tool-format block.
            const CAP_BYTES: usize = 3500;
            let raw = out.stdout_summary;
            if raw.len() <= CAP_BYTES {
                Some(raw)
            } else {
                let mut clipped = raw[..CAP_BYTES].to_string();
                clipped.push_str("\n…[orientation truncated]\n");
                Some(clipped)
            }
        }
        _ => None,
    }
}

/// POSIX-style single-quote escape: `it's fine` -> `'it'\''s fine'`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        s.to_string()
    } else {
        format!("{}…", &s[..cap])
    }
}

fn split_total_marker(raw: &str) -> (String, Option<u32>) {
    // Find the last line that is exactly "::LINES_TOTAL::N".
    let marker = "::LINES_TOTAL::";
    if let Some(idx) = raw.rfind(marker) {
        let (before, rest) = raw.split_at(idx);
        let n: Option<u32> = rest
            .trim_start_matches(marker)
            .trim()
            .parse()
            .ok();
        return (before.trim_end_matches('\n').to_string(), n);
    }
    (raw.to_string(), None)
}

fn parse_grep_line(line: &str) -> Option<Value> {
    // grep -n format: "<file>:<line>:<text>"
    let mut parts = line.splitn(3, ':');
    let file = parts.next()?.to_string();
    let line_no: u32 = parts.next()?.parse().ok()?;
    let text = parts.next()?.to_string();
    Some(json!({ "file": file, "line": line_no, "text": text }))
}

/// Tiny base64 encoder (RFC 4648) — we don't need a crate just for one
/// helper. Used to ship file content through a shell heredoc safely.
fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHA[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// MP-14a counterpart to base64_encode: decode standard RFC 4648
/// base64 (with optional `=` padding, whitespace skipped). Used by
/// edit_file to round-trip file content read back from the container.
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '=' {
            break;
        }
        let v: u32 = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err(format!("invalid base64 character: {ch:?}")),
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buf.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    Ok(buf)
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
                HarborAgentOpts::for_tests(),
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
                HarborAgentOpts {
                    turn_budget: 3,
                    ..HarborAgentOpts::for_tests()
                },
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
