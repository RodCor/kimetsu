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
    /// MP-17c: fire a one-time self-verify reminder right before the
    /// model's first finish attempt. Disabled in tests so scripted
    /// MockProvider responses aren't perturbed.
    pub self_verify_nudge_enabled: bool,
}

impl Default for HarborAgentOpts {
    fn default() -> Self {
        Self {
            turn_budget: DEFAULT_MODEL_TURN_BUDGET,
            auto_orient: true,
            min_actions_before_finish: 3,
            self_verify_nudge_enabled: true,
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
            self_verify_nudge_enabled: false,
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
         - read_file:    {{path, offset?, limit?, max_lines?}} -> {{content, lines_total, truncated}}\n\
                         (use offset+limit to read a SLICE of a big file — much cheaper than full read)\n\
         - multi_read:   {{paths:[...]}} or {{files:[{{path, offset?, limit?}}]}} -> {{files:[...]}}\n\
                         (batch N file reads in one call; cheaper than N read_file calls)\n\
         - list_files:   {{path?, max_depth?, max_entries?}} -> {{entries}}\n\
         - glob:         {{pattern, path?, max_entries?}} -> {{matches}}\n\
                         (find by path pattern: '**/*.rs', 'src/**/test_*.py')\n\
         - search_files: {{pattern, path?, max_matches?, glob?}} -> {{matches}}\n\
                         (grep file CONTENT; use `glob` instead when you only need names)\n\
         - edit_file:    {{path, old_string, new_string, replace_all?}} or {{path, edits:[...]}} -> {{bytes_delta}}\n\
         - write_file:   {{path, content}} -> {{path, bytes_written}}  (use ONLY for new files / full rewrites)\n\
         - apply_patch:  {{diff, cwd?, strip?}} -> {{files_patched, hunks_failed}}  (unified diff across files)\n\
         - move_file:    {{from, to}} -> {{moved}}\n\
         - delete_file:  {{path, recursive?}} -> {{deleted}}\n\
         - git_status:   {{cwd?}} -> {{porcelain}}\n\
         - git_diff:     {{paths?, cwd?}} -> {{diff}}\n\
         - shell_command:{{program, args, cwd_relative?, timeout_secs?}} -> {{exit_code, stdout, stderr}}\n\
         - shell_background:{{program, args, cwd_relative?}} -> {{handle, pid}}\n\
                         (fire-and-forget for long builds / training; non-blocking)\n\
         - shell_status: {{handle}} -> {{running, runtime_sec, exit_code?, bytes_stdout, bytes_stderr}}\n\
         - shell_output: {{handle, tail_bytes?}} -> {{stdout_tail, stderr_tail}}\n\
                         (poll latest output without blocking the background process)\n\
         - shell_stop:   {{handle, signal?}} -> {{stopped, exit_code}}\n\
         - view_image:   {{path, include_base64?, max_bytes?}} -> {{format, width, height, size_bytes, sha256, base64?}}\n\
                         (read image metadata + optional bytes for workspace images)\n\
         - plan:         {{todos:[{{content,status,activeForm?}}]}} -> {{todos}}\n\
                         (maintain a checklist across turns on multi-step tasks)\n\
         - think:        {{thought}} -> {{ack:true}}\n\
                         (pure deliberation slot; no I/O, no state change)\n\
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
         === Tool selection heuristics (read this before responding) ===\n\
         \n\
         LONG-RUNNING COMMANDS (>60s expected):\n\
         Use shell_background, NOT shell_command. The wrapper kills any\n\
         single foreground call that runs longer than ~1500s wall-clock,\n\
         so big builds / training / large test suites MUST be backgrounded.\n\
         Examples that need shell_background:\n\
           - `make`, `make -j`, `cargo build --release`, `cmake --build`\n\
           - `pip install` of heavy packages, `npm install` in big trees\n\
           - Training scripts (python train.py, caffe train ...)\n\
           - Long test suites, ray tracers, simulators\n\
           - Anything where you'd watch a progress bar\n\
         Pattern:\n\
           1) shell_background {{program, args}}        -> {{handle, pid}}\n\
           2) shell_status     {{handle}}                -> {{running, runtime_sec, exit_code?}}\n\
           3) shell_output     {{handle, tail_bytes:4096}} -> {{stdout_tail, stderr_tail}}\n\
              Loop 2+3 as needed. Insert `think` calls between polls to\n\
              plan next steps instead of polling tighter than ~10s.\n\
           4) shell_stop {{handle}} only if you decide to give up early.\n\
         \n\
         FILE EDITS — pick the cheapest tool that fits:\n\
           - edit_file: changing a few lines in an existing file. ~50x\n\
             cheaper than write_file for small edits. Hash-checked.\n\
           - apply_patch: coordinated changes across multiple files.\n\
             Accepts BOTH standard unified diff AND Codex starred-patch\n\
             format (*** Begin Patch / *** Update File: / *** Add File:).\n\
           - write_file: ONLY for new files or full rewrites. Using it\n\
             to change a few lines wastes tokens and risks corrupting\n\
             unrelated content.\n\
         \n\
         FILE READS — slice big files, don't dump them:\n\
           - read_file {{offset, limit}}: read lines N..M of a big file.\n\
             A 2000-line source dump costs ~10k tokens of input;\n\
             lines 400-450 costs ~50 tokens. Pick the slice.\n\
           - multi_read: 2+ files at once in one tool call. Cheaper than\n\
             N separate read_file calls because the shell round-trip is\n\
             paid once.\n\
           - glob: find files by NAME pattern ('**/*.py'). Use this\n\
             instead of search_files when you don't need content search.\n\
           - search_files: grep file CONTENT. Slower than glob; only use\n\
             when you need to find code by what it contains.\n\
         \n\
         PLANNING & DELIBERATION:\n\
           - plan: when the task has >=3 distinct steps, call plan FIRST\n\
             with the breakdown. Update statuses as you progress. The\n\
             plan survives across turns in your conversation history.\n\
           - think: pure reasoning slot — no I/O. Use when you need to\n\
             work through a problem before acting. Cheaper than a probe\n\
             tool call.\n\
         \n\
         PARALLEL CALLS (when independent):\n\
         Batch reads / status checks / unrelated edits via the parallel\n\
         tool_calls form. Saves one full model turn per extra call.\n\
         Don't batch dependent calls (e.g. read after edit) — those need\n\
         to be serialized.\n\
         \n\
         === Worked example: long-running compile task ===\n\
         Task summary: \"Build CompCert from source, expose ccomp binary.\"\n\
         Turn 1 (orient + read instructions in one shot):\n\
           {{\"thought\":\"sniff layout and INSTALL doc\",\n\
            \"tool_calls\":[\n\
              {{\"name\":\"list_files\",\"input\":{{\"path\":\".\",\"max_depth\":1}}}},\n\
              {{\"name\":\"read_file\",\"input\":{{\"path\":\"INSTALL.md\",\"limit\":80}}}}\n\
            ]}}\n\
         Turn 2 (configure + record plan):\n\
           {{\"thought\":\"configure first, then background make\",\n\
            \"tool_calls\":[\n\
              {{\"name\":\"shell_command\",\"input\":{{\"program\":\"./configure\",\"args\":[\"x86_64-linux\"]}}}},\n\
              {{\"name\":\"plan\",\"input\":{{\"todos\":[\n\
                {{\"content\":\"configure\",\"status\":\"completed\"}},\n\
                {{\"content\":\"build (background)\",\"status\":\"in_progress\"}},\n\
                {{\"content\":\"verify ccomp binary\",\"status\":\"pending\"}}\n\
              ]}}}}\n\
            ]}}\n\
         Turn 3 (kick off background build):\n\
           {{\"thought\":\"make -j; expect ~15 min\",\n\
            \"tool_call\":{{\"name\":\"shell_background\",\"input\":{{\"program\":\"make\",\"args\":[\"-j4\"]}}}}}}\n\
         Turn 4 (poll while waiting):\n\
           {{\"thought\":\"check progress\",\n\
            \"tool_call\":{{\"name\":\"shell_status\",\"input\":{{\"handle\":\"bg-<received>\"}}}}}}\n\
         Turn 5 (still running, check tail):\n\
           {{\"thought\":\"see latest output\",\n\
            \"tool_call\":{{\"name\":\"shell_output\",\"input\":{{\"handle\":\"bg-<received>\",\"tail_bytes\":4096}}}}}}\n\
         Turn N (build done, verify):\n\
           {{\"thought\":\"build complete, smoke-test ccomp\",\n\
            \"tool_call\":{{\"name\":\"shell_command\",\"input\":{{\"program\":\"/tmp/CompCert/ccomp\",\"args\":[\"-v\"]}}}}}}\n\
         Turn N+1 (finish):\n\
           {{\"thought\":\"ccomp prints version; done\",\n\
            \"finish\":{{\"summary\":\"CompCert built; ccomp at /tmp/CompCert/ccomp\"}}}}\n\
         \n\
         === Common pitfalls (don't do these) ===\n\
           - Running `make` via shell_command for a multi-minute build\n\
             (it will time out — use shell_background).\n\
           - Using write_file to change 3 lines (use edit_file).\n\
           - Reading a 2000-line file when you only need ~50 lines\n\
             (use offset+limit).\n\
           - Polling shell_status every 1s without a `think` between\n\
             polls (wasteful; the model burns turns on no-op probes).\n\
           - Emitting plain text without a JSON envelope when you have\n\
             more work to do (Kimetsu will nudge you to keep going).\n\
           - Calling finish before producing the artifact the verifier\n\
             expects (the verifier reads files / runs scripts; an empty\n\
             summary alone won't pass).\n\
         \n\
         Workspace paths are relative to the task's starting directory.\n\
         Begin with one tool_call (or one parallel tool_calls batch).\n\
         Do not narrate. Do not output plain prose. Always wrap in JSON."
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
    // MP-17c: flag so the self-verify nudge fires at most once per task.
    let mut self_verify_nudge_sent = false;

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

        // MP-17c: one-time pre-finish self-verify nudge. Fires AT MOST ONCE
        // per task, after the persistence gate has passed. On the
        // make-mips-interpreter / install-windows-3-11 / video-processing
        // class of failures the model produced output that didn't match
        // the verifier; a single "before I accept finish, briefly check
        // your output exists and matches the spec" reminder costs ~one
        // extra model turn but catches the "I forgot to verify" cases
        // cheaply. The model is free to confirm "verified, done" and
        // re-emit finish on the next turn — we accept it then unconditionally.
        if !self_verify_nudge_sent && opts.self_verify_nudge_enabled {
            self_verify_nudge_sent = true;
            let nudge = "Before I accept the finish: briefly verify your \
                output is what the verifier will check. Concretely: list \
                the files you produced (list_files / shell_command ls), \
                cat at least one to confirm content, and run any \
                verification command the task spec mentions (test runner, \
                a known executable, the named binary). If everything \
                checks out, emit `finish` on your next turn and we're \
                done. If you spot a gap, fix it first.";
            if let Some(text) = response.text.clone() {
                messages.push(ModelMessage::assistant_text(text));
            }
            messages.push(ModelMessage::user_text(nudge.to_string()));
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
            description: "Read a UTF-8 text file from the workspace. Returns content, \
                total line count, and a `truncated` flag if the slice was capped. \
                MP-14e: use `offset` (1-based start line) and `limit` (max lines to \
                return) to read a SLICE of a large file — far cheaper in input \
                tokens than fetching the whole thing. `max_lines` (legacy) caps the \
                slice from line 1; prefer `offset`+`limit` for big files."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "1-based start line. Default 1." },
                    "limit": { "type": "integer", "description": "Max lines to return from `offset`. Default 800." },
                    "max_lines": { "type": "integer", "description": "Legacy: cap on lines from line 1. Use offset+limit instead." }
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
            description: "Apply a diff to one or more files. Accepts EITHER \
                standard unified-diff format (`--- a/old\\n+++ b/new\\n@@ ...`) \
                OR Codex starred-patch format (MP-16b):\n  \
                  *** Begin Patch\n  \
                  *** Update File: path/to/file\n  \
                  @@ optional context\n  \
                  -old line\n  \
                  +new line\n  \
                  *** End Patch\n\
                Also supports `*** Add File: path` (followed by `+`-prefixed \
                lines for new content) and `*** Delete File: path` for \
                removals. `strip` defaults to 0 (workspace-relative paths); \
                set to 1 if your diff has `a/` / `b/` prefixes. Returns the \
                list of files patched and any hunk failures."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "diff": { "type": "string", "description": "Unified diff OR Codex starred-patch text." },
                    "cwd": { "type": "string", "description": "Workspace-relative directory to apply the patch in; default workspace root." },
                    "strip": { "type": "integer", "description": "patch -p<N> prefix-strip. Default 0 (ignored for Codex starred-patch)." }
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
        // ---------- MP-14e additions ----------
        ToolDefinition {
            name: "glob".to_string(),
            description: "Find files whose path matches a glob pattern (cheap \
                pattern-based discovery; complements search_files which greps \
                content). Supports `**` for recursive descent. Example: \
                `{pattern: \"**/*.rs\"}`. Returns matching paths sorted by \
                modification time (most recent first) so the model sees what \
                changed most recently first."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob like '**/*.py' or 'src/**/test_*.go'." },
                    "path": { "type": "string", "description": "Workspace-relative root; default '.'." },
                    "max_entries": { "type": "integer", "description": "Default 200." }
                },
                "required": ["pattern"],
            }),
        },
        ToolDefinition {
            name: "multi_read".to_string(),
            description: "Read several files in a single tool call. Cheaper than \
                N separate read_file calls because the shell round-trip cost is \
                paid once. Each entry can carry its own offset/limit slice. \
                Returns `files: [{path, content, lines_total, truncated, error?}]`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "description": "Simple form: array of workspace-relative paths. Each read uses full default slice.",
                        "items": { "type": "string" }
                    },
                    "files": {
                        "type": "array",
                        "description": "Rich form: array of {path, offset?, limit?} for per-file slicing.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "offset": { "type": "integer" },
                                "limit": { "type": "integer" }
                            },
                            "required": ["path"],
                        }
                    },
                    "limit": { "type": "integer", "description": "Default per-file slice cap when using `paths`. Default 400." }
                },
            }),
        },
        ToolDefinition {
            name: "move_file".to_string(),
            description: "Rename or move a file or directory inside the workspace. \
                Refuses absolute paths, `..` traversal, and empty operands."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Workspace-relative source path." },
                    "to":   { "type": "string", "description": "Workspace-relative destination path." }
                },
                "required": ["from", "to"],
            }),
        },
        ToolDefinition {
            name: "delete_file".to_string(),
            description: "Delete a file or directory under the workspace. Set \
                `recursive: true` for directories. Refuses absolute paths, \
                `..` traversal, and the workspace root itself."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "description": "Required for non-empty directories. Default false." }
                },
                "required": ["path"],
            }),
        },
        ToolDefinition {
            name: "plan".to_string(),
            description: "Record or update a structured plan / todo list for this \
                task. The plan is echoed back in the tool result and remains in \
                the conversation history, giving you a visible checklist across \
                turns. Use it on multi-step tasks: enumerate the steps up front, \
                then update statuses as you go. Status must be one of: pending, \
                in_progress, completed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Imperative step description." },
                                "status":  { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                "activeForm": { "type": "string", "description": "Optional present-continuous form for display." }
                            },
                            "required": ["content", "status"],
                        }
                    }
                },
                "required": ["todos"],
            }),
        },
        ToolDefinition {
            name: "think".to_string(),
            description: "Pure deliberation slot: pass a `thought` string, get an \
                acknowledgement back. No shell, no I/O, no state change. Use \
                this when you need to reason about what to do next without \
                burning a tool call on an action. Cheap; the thought is \
                preserved in the conversation history."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thought": { "type": "string", "description": "Free-form reasoning, plan, or observation." }
                },
                "required": ["thought"],
            }),
        },
        // ---------- MP-16a: background shell quartet ----------
        // For long-running tasks (builds, training, multi-step compilers)
        // where a single foreground shell_command would blow the
        // claude_code provider wall-clock (MP-15a default 1500s). Pattern:
        // fire shell_background -> get handle -> poll shell_status /
        // shell_output every model turn -> shell_stop if needed. State
        // lives in /tmp/kimetsu-bg-<handle>.{meta,out,err,exitcode} so the
        // composed-shell model has continuity across tool calls.
        ToolDefinition {
            name: "shell_background".to_string(),
            description: "Spawn a long-running command in the background and \
                return a handle. The process runs detached; stdout/stderr \
                land in /tmp/kimetsu-bg-<handle>.{out,err}. Use this for \
                anything you'd otherwise want to leave running while you \
                think (compiles, training, large test suites). Poll with \
                shell_status / shell_output; reap with shell_stop or by \
                letting it exit naturally."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "program": { "type": "string" },
                    "args":    { "type": "array", "items": { "type": "string" } },
                    "cwd_relative": { "type": "string", "description": "Optional workspace-relative cwd." }
                },
                "required": ["program"],
            }),
        },
        ToolDefinition {
            name: "shell_status".to_string(),
            description: "Check whether a background process from \
                shell_background is still running. Returns \
                {running, runtime_sec, exit_code?, bytes_stdout, bytes_stderr}. \
                Non-blocking — call it freely."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" }
                },
                "required": ["handle"],
            }),
        },
        ToolDefinition {
            name: "shell_output".to_string(),
            description: "Fetch the latest stdout/stderr from a background \
                process. `tail_bytes` (default 8192) caps the size returned \
                so a chatty build doesn't blow the response budget. The \
                process keeps running; this is read-only."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle":     { "type": "string" },
                    "tail_bytes": { "type": "integer", "description": "Default 8192." }
                },
                "required": ["handle"],
            }),
        },
        ToolDefinition {
            name: "shell_stop".to_string(),
            description: "Send a signal to a background process. Default \
                SIGTERM (15); pass `signal: 9` for SIGKILL. Returns \
                {stopped, exit_code, runtime_sec}. After stop, log files \
                remain until the task ends."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" },
                    "signal": { "type": "integer", "description": "POSIX signal number; default 15 (SIGTERM)." }
                },
                "required": ["handle"],
            }),
        },
        // ---------- MP-16c: view_image ----------
        ToolDefinition {
            name: "view_image".to_string(),
            description: "Read an image file from the workspace and return \
                metadata plus optional base64 content. Useful when a task \
                ships PNG/JPEG/PDF/etc. as part of the workspace and you \
                need to verify size / hash / dimensions, or pipe the bytes \
                to a script. By default returns metadata only; set \
                `include_base64: true` to also return the bytes (capped at \
                `max_bytes`, default 256KB)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":           { "type": "string" },
                    "include_base64": { "type": "boolean", "description": "Default false (metadata only)." },
                    "max_bytes":      { "type": "integer", "description": "Cap when include_base64=true; default 262144." }
                },
                "required": ["path"],
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
        // MP-14e additions
        "glob" => harbor_glob(runtime, &input),
        "multi_read" => harbor_multi_read(runtime, &input),
        "move_file" => harbor_move_file(runtime, &input),
        "delete_file" => harbor_delete_file(runtime, &input),
        "plan" => harbor_plan(&input),
        "think" => harbor_think(&input),
        // MP-16 additions
        "shell_background" => harbor_shell_background(runtime, &input),
        "shell_status"     => harbor_shell_status(runtime, &input),
        "shell_output"     => harbor_shell_output(runtime, &input),
        "shell_stop"       => harbor_shell_stop(runtime, &input),
        "view_image"       => harbor_view_image(runtime, &input),
        other => json!({
            "error": format!(
                "unsupported tool `{other}`; pick one of \
                 read_file / list_files / search_files / write_file / \
                 edit_file / apply_patch / git_status / git_diff / shell_command / \
                 glob / multi_read / move_file / delete_file / plan / think / \
                 shell_background / shell_status / shell_output / shell_stop / \
                 view_image"
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
    // MP-14e: offset/limit semantics, with backwards-compat to max_lines.
    // - explicit `offset`/`limit` wins
    // - else fall back to max_lines from line 1
    // - both defaults: 1..=800
    let has_slice = input.get("offset").is_some() || input.get("limit").is_some();
    let offset = input_u32(input, "offset", 1).max(1);
    let limit = if has_slice {
        input_u32(input, "limit", READ_FILE_DEFAULT_MAX_LINES)
    } else {
        input_u32(input, "max_lines", READ_FILE_DEFAULT_MAX_LINES)
    };
    let end_line = offset.saturating_add(limit).saturating_sub(1);

    // `wc -l` first so we know how truncated we are; then sed -n to slice.
    // Both run in one shell so the model only sees one tool turn.
    let cmd = format!(
        "set -e; total=$(wc -l < {0} 2>/dev/null || echo 0); sed -n '{1},{2}p' {0}; echo \"::LINES_TOTAL::$total\"",
        shell_quote(path),
        offset,
        end_line,
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
    // truncated: there are more lines beyond what we returned.
    let truncated = total_line_count
        .map(|t| t > offset.saturating_add(returned_lines).saturating_sub(1))
        .unwrap_or(false);
    json!({
        "path": path,
        "content": content,
        "offset": offset,
        "limit": limit,
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
    let Some(diff_raw) = input_str(input, "diff") else {
        return json!({ "error": "apply_patch requires `diff` (unified-diff or Codex starred-patch text)" });
    };
    let cwd = input_str(input, "cwd").map(str::to_string);
    let mut strip = input_u32(input, "strip", 0);

    // MP-16b: detect Codex's starred-patch format and translate to
    // standard unified diff. The Codex format opens with `*** Begin
    // Patch`; the translator produces `--- a/X` / `+++ b/X` headers so
    // GNU patch accepts it, which forces strip=1 regardless of the
    // caller's `strip` arg.
    let (diff, format_used) = if diff_raw.contains("*** Begin Patch") {
        match translate_codex_patch(diff_raw) {
            Ok(translated) => {
                strip = 1;
                (translated, "codex_starred")
            }
            Err(err) => {
                return json!({
                    "error": format!("apply_patch: Codex starred-patch translation failed: {err}"),
                });
            }
        }
    } else {
        (diff_raw.to_string(), "unified")
    };

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

    // MP-17 #10: if GNU `patch` is not installed in this container, fall
    // back to a Rust-side unified-diff applier that re-routes each hunk
    // through harbor_edit_file (base64 round-trip read+edit+write through
    // the composed-shell layer). Detection: exit 127 ("command not found"
    // by convention) or stderr explicitly says so.
    let patch_not_found = out.exit_code == 127
        || out.stderr_summary.contains("command not found")
        || out.stdout_summary.contains("command not found");
    if patch_not_found {
        return harbor_apply_patch_rust_fallback(runtime, &diff, cwd.as_deref(), format_used);
    }

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
            "format": format_used,
            "files_attempted": patched_files,
            "hunks_failed": hunks_failed,
            "rejects": rejects,
            "exit_code": out.exit_code,
        });
    }

    json!({
        "ok": true,
        "format": format_used,
        "files_patched": patched_files,
        "hunks_failed": hunks_failed,
        "rejects": rejects,
        "output_excerpt": truncate(&out.stdout_summary, 800),
    })
}

// ---------------------------------------------------------------------------
// MP-17 #10: pure-Rust unified-diff applier (fallback when patch(1) absent).
//
// Strategy: parse the diff into per-file sections + per-hunk old/new pairs,
// then re-route each hunk through harbor_edit_file. edit_file already does
// the base64 round-trip read+modify+write through HarborShellExecutor, so
// we get composed-shell safety for free and don't depend on patch(1) being
// installed in the container.
//
// Limitations vs GNU patch:
//   - No fuzz matching: the reconstructed OLD must occur in the file
//     exactly once (edit_file's hash-check semantics).
//   - No --reverse / --merge mode.
//   - No automatic .rej generation; the response carries `files_failed`
//     entries so the model can self-correct.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DiffFile {
    path: String,
    /// is_new: true if header was `--- /dev/null` (creation)
    is_new: bool,
    /// is_delete: true if header was `+++ /dev/null` (deletion)
    is_delete: bool,
    hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone)]
struct DiffHunk {
    /// Reconstructed OLD text (context + removal lines).
    old: String,
    /// Reconstructed NEW text (context + addition lines).
    new: String,
}

fn parse_unified_diff(diff: &str) -> Result<Vec<DiffFile>, String> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut iter = diff.lines().peekable();
    while let Some(line) = iter.next() {
        if !line.starts_with("--- ") {
            continue;
        }
        let minus_header = line;
        let plus_header = iter.next().unwrap_or("");
        if !plus_header.starts_with("+++ ") {
            return Err(format!(
                "expected `+++ ` after `--- `, got: {}",
                truncate(plus_header, 80)
            ));
        }
        let is_new = minus_header.trim_start_matches("--- ").trim() == "/dev/null";
        let is_delete = plus_header.trim_start_matches("+++ ").trim() == "/dev/null";
        let path = if is_delete {
            extract_diff_path(minus_header.trim_start_matches("--- "))
        } else {
            extract_diff_path(plus_header.trim_start_matches("+++ "))
        };
        let mut hunks = Vec::new();
        // Walk lines until the next `--- ` (next file) or EOF, collecting hunks.
        while let Some(next) = iter.peek() {
            if next.starts_with("--- ") {
                break;
            }
            let line = iter.next().unwrap();
            if line.starts_with("@@") {
                // Start a new hunk; collect body until next `@@` or `--- `.
                let mut old_lines = String::new();
                let mut new_lines = String::new();
                while let Some(peek) = iter.peek() {
                    if peek.starts_with("@@") || peek.starts_with("--- ") {
                        break;
                    }
                    let body = iter.next().unwrap();
                    if let Some(rest) = body.strip_prefix('-') {
                        if body.starts_with("---") {
                            break;
                        }
                        old_lines.push_str(rest);
                        old_lines.push('\n');
                    } else if let Some(rest) = body.strip_prefix('+') {
                        if body.starts_with("+++") {
                            break;
                        }
                        new_lines.push_str(rest);
                        new_lines.push('\n');
                    } else if let Some(rest) = body.strip_prefix(' ') {
                        old_lines.push_str(rest);
                        old_lines.push('\n');
                        new_lines.push_str(rest);
                        new_lines.push('\n');
                    } else if body.is_empty() {
                        old_lines.push('\n');
                        new_lines.push('\n');
                    } else if body.starts_with('\\') {
                        // "\ No newline at end of file" — skip the marker.
                    } else {
                        // Unknown line type; treat as context to be safe.
                        old_lines.push_str(body);
                        old_lines.push('\n');
                        new_lines.push_str(body);
                        new_lines.push('\n');
                    }
                }
                hunks.push(DiffHunk {
                    old: old_lines,
                    new: new_lines,
                });
            }
        }
        files.push(DiffFile {
            path,
            is_new,
            is_delete,
            hunks,
        });
    }
    if files.is_empty() {
        return Err("no per-file sections found (no `--- ` headers)".to_string());
    }
    Ok(files)
}

fn extract_diff_path(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip GNU patch's a/ or b/ prefix when present.
    if let Some(rest) = trimmed.strip_prefix("a/") {
        return rest.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("b/") {
        return rest.to_string();
    }
    trimmed.to_string()
}

fn harbor_apply_patch_rust_fallback(
    runtime: &mut crate::tools::ToolRuntime,
    diff: &str,
    cwd: Option<&str>,
    format_used: &'static str,
) -> Value {
    let files = match parse_unified_diff(diff) {
        Ok(f) => f,
        Err(e) => {
            return json!({
                "error": format!("apply_patch (rust fallback): diff parse failed: {e}"),
                "format": format_used,
            });
        }
    };

    let mut files_patched: Vec<String> = Vec::new();
    let mut files_failed: Vec<Value> = Vec::new();
    let mut hunks_failed = 0u32;

    for f in files {
        let full_path = match cwd {
            Some(c) if !c.is_empty() => format!("{}/{}", c.trim_end_matches('/'), f.path),
            _ => f.path.clone(),
        };

        if f.is_new {
            // Reconstruct the new-file content from the hunks' `new` text.
            let content: String = f.hunks.iter().map(|h| h.new.clone()).collect();
            let write_input = json!({ "path": full_path, "content": content });
            let res = harbor_write_file(runtime, &write_input);
            if res.get("error").is_some() {
                files_failed.push(json!({
                    "path": full_path,
                    "kind": "add_file",
                    "error": res["error"].clone(),
                }));
            } else {
                files_patched.push(full_path);
            }
            continue;
        }
        if f.is_delete {
            let del_input = json!({ "path": full_path, "recursive": false });
            let res = harbor_delete_file(runtime, &del_input);
            if res.get("error").is_some() {
                files_failed.push(json!({
                    "path": full_path,
                    "kind": "delete_file",
                    "error": res["error"].clone(),
                }));
            } else {
                files_patched.push(full_path);
            }
            continue;
        }
        // Modify case: run each hunk through edit_file in order.
        let mut file_ok = true;
        for (i, hunk) in f.hunks.iter().enumerate() {
            // Skip trailing newline difference quirks: edit_file uses exact match.
            let edit_input = json!({
                "path": full_path,
                "old_string": hunk.old.trim_end_matches('\n'),
                "new_string": hunk.new.trim_end_matches('\n'),
            });
            let res = harbor_edit_file(runtime, &edit_input);
            if res.get("error").is_some() {
                files_failed.push(json!({
                    "path": full_path,
                    "hunk": i,
                    "error": res["error"].clone(),
                }));
                hunks_failed += 1;
                file_ok = false;
                // Don't bail on the file — subsequent hunks may still apply.
            }
        }
        if file_ok {
            files_patched.push(full_path);
        }
    }

    if files_patched.is_empty() && !files_failed.is_empty() {
        return json!({
            "error": "apply_patch (rust fallback): no files patched cleanly",
            "format": format_used,
            "fallback": "rust",
            "files_patched": files_patched,
            "files_failed": files_failed,
            "hunks_failed": hunks_failed,
        });
    }

    json!({
        "ok": files_failed.is_empty(),
        "format": format_used,
        "fallback": "rust",
        "files_patched": files_patched,
        "files_failed": files_failed,
        "hunks_failed": hunks_failed,
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

// ---------------------------------------------------------------------------
// MP-14e: pattern file-find, batch read, typed move/delete, plan, think.
// All six exist to cut per-turn cost or per-turn waste vs composing the same
// thing out of raw shell_command calls.
// ---------------------------------------------------------------------------

const GLOB_DEFAULT_MAX_ENTRIES: u32 = 200;
const MULTI_READ_DEFAULT_LIMIT: u32 = 400;
const MULTI_READ_MAX_FILES: usize = 25;

/// MP-14e: glob — pattern-based file discovery (complement to search_files's
/// content grep). Maps the glob to `find -path`. `**` is rewritten to `*`
/// because POSIX find doesn't grok `**` natively; we still recurse because
/// `find` recurses by default unless `-maxdepth` says otherwise.
fn harbor_glob(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(pattern) = input_str(input, "pattern") else {
        return json!({ "error": "glob requires `pattern`" });
    };
    let path = input_str(input, "path").unwrap_or(".");
    let max_entries = input_u32(input, "max_entries", GLOB_DEFAULT_MAX_ENTRIES);
    // POSIX find treats `**` as `*/*` rather than recursive-everything. We
    // recurse by default; collapse `**` to `*` for matching purposes.
    let find_pattern = pattern.replace("**", "*");
    // Use `find ... -path '*<pat>'` so a relative pattern like '*.rs' still
    // matches files in nested directories.
    let path_pattern = if find_pattern.starts_with('/') || find_pattern.starts_with("./") {
        find_pattern.clone()
    } else {
        format!("*/{find_pattern}")
    };
    // -printf '%T@ %p\n' gives mtime-prefixed paths for sort -nr.
    let cmd = format!(
        "find {0} -type f \\( -path {1} -o -path {2} \\) -printf '%T@ %p\\n' 2>/dev/null \
         | sort -nr | head -n {3} | cut -d' ' -f2-",
        shell_quote(path),
        shell_quote(&path_pattern),
        shell_quote(&find_pattern),
        max_entries
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), cmd],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("glob shell failed: {e}") }),
    };
    let matches: Vec<&str> = out
        .stdout_summary
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    let returned = matches.len() as u32;
    json!({
        "pattern": pattern,
        "path": path,
        "matches": matches,
        "matches_returned": returned,
        "truncated": returned >= max_entries,
    })
}

/// MP-14e: multi_read — batch N file reads in one tool call. Accepts either
/// `paths: ["a", "b"]` (each with the default slice) or `files: [{path,
/// offset?, limit?}]` (per-file slice). Caps at MULTI_READ_MAX_FILES to keep
/// any single result envelope bounded.
fn harbor_multi_read(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    #[derive(Debug)]
    struct Req {
        path: String,
        offset: u32,
        limit: u32,
    }

    let default_limit = input_u32(input, "limit", MULTI_READ_DEFAULT_LIMIT);
    let mut reqs: Vec<Req> = Vec::new();

    if let Some(arr) = input.get("paths").and_then(Value::as_array) {
        for v in arr {
            if let Some(p) = v.as_str() {
                reqs.push(Req {
                    path: p.to_string(),
                    offset: 1,
                    limit: default_limit,
                });
            }
        }
    }
    if let Some(arr) = input.get("files").and_then(Value::as_array) {
        for v in arr {
            let Some(path) = v.get("path").and_then(Value::as_str) else { continue };
            let offset = v
                .get("offset")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(1)
                .max(1);
            let limit = v
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or(default_limit);
            reqs.push(Req {
                path: path.to_string(),
                offset,
                limit,
            });
        }
    }
    if reqs.is_empty() {
        return json!({ "error": "multi_read requires `paths` or `files`" });
    }
    if reqs.len() > MULTI_READ_MAX_FILES {
        return json!({
            "error": format!(
                "multi_read accepts at most {MULTI_READ_MAX_FILES} files per call (got {})",
                reqs.len()
            )
        });
    }
    let mut files: Vec<Value> = Vec::with_capacity(reqs.len());
    for r in &reqs {
        let inner = json!({
            "path": r.path,
            "offset": r.offset,
            "limit": r.limit,
        });
        let result = harbor_read_file(runtime, &inner);
        files.push(result);
    }
    json!({
        "files": files,
        "files_returned": files.len() as u32,
    })
}

/// MP-14e: move_file — typed rename inside the workspace. Refuses absolute
/// paths and `..` traversal so a typo can't escape the sandbox.
fn harbor_move_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(from) = input_str(input, "from") else {
        return json!({ "error": "move_file requires `from`" });
    };
    let Some(to) = input_str(input, "to") else {
        return json!({ "error": "move_file requires `to`" });
    };
    if let Err(err) = check_workspace_path("from", from) {
        return json!({ "error": err });
    }
    if let Err(err) = check_workspace_path("to", to) {
        return json!({ "error": err });
    }
    let out = match run_shell(
        runtime,
        "mv",
        vec![from.to_string(), to.to_string()],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("move_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("move_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "from": from,
            "to": to,
        });
    }
    json!({ "moved": true, "from": from, "to": to })
}

/// MP-14e: delete_file — typed remove inside the workspace. `recursive: true`
/// is required for non-empty directories. Refuses workspace root, absolute
/// paths, and `..` traversal.
fn harbor_delete_file(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "delete_file requires `path`" });
    };
    if path == "." || path == "./" {
        return json!({ "error": "delete_file: refusing to delete the workspace root" });
    }
    if let Err(err) = check_workspace_path("path", path) {
        return json!({ "error": err });
    }
    let recursive = input
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut args: Vec<String> = Vec::with_capacity(2);
    if recursive {
        args.push("-rf".to_string());
    } else {
        args.push("-f".to_string());
    }
    args.push(path.to_string());
    let out = match run_shell(
        runtime,
        "rm",
        args,
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("delete_file shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("delete_file: exit {}; stderr: {}", out.exit_code, truncate(&out.stderr_summary, 400)),
            "path": path,
        });
    }
    json!({ "deleted": true, "path": path, "recursive": recursive })
}

/// MP-14e: plan — record a todo list. We don't persist it server-side; the
/// tool simply validates + echoes it. The fact that the result sits in the
/// conversation history is enough for the model to "see" the plan on every
/// subsequent turn. Status must be pending|in_progress|completed.
fn harbor_plan(input: &Value) -> Value {
    let Some(todos) = input.get("todos").and_then(Value::as_array) else {
        return json!({ "error": "plan requires `todos: [{content,status,activeForm?}]`" });
    };
    let mut normalized: Vec<Value> = Vec::with_capacity(todos.len());
    let mut counts = (0u32, 0u32, 0u32); // (pending, in_progress, completed)
    for (i, item) in todos.iter().enumerate() {
        let Some(content) = item.get("content").and_then(Value::as_str) else {
            return json!({ "error": format!("plan: todo[{i}] missing `content`") });
        };
        let Some(status) = item.get("status").and_then(Value::as_str) else {
            return json!({ "error": format!("plan: todo[{i}] missing `status`") });
        };
        match status {
            "pending" => counts.0 += 1,
            "in_progress" => counts.1 += 1,
            "completed" => counts.2 += 1,
            other => {
                return json!({
                    "error": format!(
                        "plan: todo[{i}] status={other:?}; must be pending|in_progress|completed"
                    )
                });
            }
        }
        let active_form = item
            .get("activeForm")
            .and_then(Value::as_str)
            .unwrap_or(content);
        normalized.push(json!({
            "content": content,
            "status": status,
            "activeForm": active_form,
        }));
    }
    json!({
        "todos": normalized,
        "pending": counts.0,
        "in_progress": counts.1,
        "completed": counts.2,
        "total": normalized.len() as u32,
    })
}

/// MP-14e: think — pure ack. No shell, no I/O. The thought lives in the
/// conversation history; that's the entire point.
fn harbor_think(input: &Value) -> Value {
    let Some(thought) = input_str(input, "thought") else {
        return json!({ "error": "think requires `thought`" });
    };
    json!({
        "ack": true,
        "thought_len": thought.len() as u32,
    })
}

/// Workspace-path safety: workspace-relative, no absolute prefix, no `..`
/// segment. Used by move_file / delete_file to keep a typo from reaching
/// outside the task sandbox.
fn check_workspace_path(label: &str, p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err(format!("`{label}` must not be empty"));
    }
    if p.starts_with('/') {
        return Err(format!("`{label}` must be workspace-relative, not absolute"));
    }
    for seg in p.split('/') {
        if seg == ".." {
            return Err(format!("`{label}` may not contain `..` segments"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MP-16b: translate Codex starred-patch -> standard unified diff.
//
// Codex grammar:
//   *** Begin Patch
//   *** Update File: <path>
//   @@ optional anchor
//   -old
//   +new
//   *** Add File: <path>
//   +line
//   +line
//   *** Delete File: <path>
//   *** End Patch
//
// We translate each per-file section into:
//   --- a/<path>
//   +++ b/<path>
//   <hunk>
// then GNU patch consumes it with -p1 (a/ / b/ prefixes stripped).
//
// This is intentionally permissive — Codex's emitted diffs vary in
// whether they include `@@` markers, blank lines, etc. We synthesize a
// generic `@@ -0,0 +1,N @@` for Add File and `@@ -1,N +0,0 @@` for
// Delete File so GNU patch is happy.
fn translate_codex_patch(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut current: Option<CodexSection> = None;
    let mut saw_begin = false;
    let mut saw_end = false;

    fn flush(section: CodexSection, out: &mut String) -> Result<(), String> {
        match section {
            CodexSection::Update { path, body } => {
                out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
                // If body doesn't include any @@ line, GNU patch in --batch
                // mode still accepts it as a flat hunk if we prepend one.
                // We compute crude counts so the hunk header isn't a lie.
                let has_hunk = body.lines().any(|l| l.starts_with("@@"));
                if !has_hunk {
                    let (minus, plus) = body.lines().fold((0u32, 0u32), |(m, p), l| {
                        if l.starts_with('-') && !l.starts_with("---") {
                            (m + 1, p)
                        } else if l.starts_with('+') && !l.starts_with("+++") {
                            (m, p + 1)
                        } else if !l.is_empty() {
                            (m + 1, p + 1) // context line counted both sides
                        } else {
                            (m, p)
                        }
                    });
                    out.push_str(&format!("@@ -1,{minus} +1,{plus} @@\n"));
                }
                out.push_str(&body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
            }
            CodexSection::Add { path, body } => {
                let plus = body.lines().count() as u32;
                out.push_str(&format!("--- /dev/null\n+++ b/{path}\n"));
                out.push_str(&format!("@@ -0,0 +1,{plus} @@\n"));
                // Body lines should already be prefixed with `+`. If not,
                // add the prefix so GNU patch reads them as additions.
                for line in body.lines() {
                    if line.starts_with('+') {
                        out.push_str(line);
                    } else {
                        out.push('+');
                        out.push_str(line);
                    }
                    out.push('\n');
                }
            }
            CodexSection::Delete { path } => {
                // Best-effort delete header. We don't know the original
                // line count; GNU patch with --batch accepts this form
                // and removes the file outright when `--remove-empty-files`
                // could be set, but to stay safe we emit a single-line
                // negative hunk.
                out.push_str(&format!("--- a/{path}\n+++ /dev/null\n"));
                out.push_str("@@ -1,1 +0,0 @@\n-\n");
            }
        }
        Ok(())
    }

    for line in raw.lines() {
        if line.starts_with("*** Begin Patch") {
            saw_begin = true;
            continue;
        }
        if line.starts_with("*** End Patch") {
            saw_end = true;
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            current = Some(CodexSection::Update {
                path: rest.trim().to_string(),
                body: String::new(),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            current = Some(CodexSection::Add {
                path: rest.trim().to_string(),
                body: String::new(),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            if let Some(sec) = current.take() {
                flush(sec, &mut out)?;
            }
            current = Some(CodexSection::Delete {
                path: rest.trim().to_string(),
            });
            continue;
        }
        if let Some(sec) = current.as_mut() {
            match sec {
                CodexSection::Update { body, .. } | CodexSection::Add { body, .. } => {
                    body.push_str(line);
                    body.push('\n');
                }
                CodexSection::Delete { .. } => { /* drop trailing lines for Delete */ }
            }
        }
    }

    if !saw_begin {
        return Err("missing `*** Begin Patch` header".to_string());
    }
    if !saw_end {
        // Permissive: if `*** End Patch` was forgotten, flush whatever
        // section was open. Don't outright fail.
        if let Some(sec) = current.take() {
            flush(sec, &mut out)?;
        }
    }
    if out.is_empty() {
        return Err("no per-file sections found".to_string());
    }
    Ok(out)
}

enum CodexSection {
    Update { path: String, body: String },
    Add    { path: String, body: String },
    Delete { path: String },
}

// ---------------------------------------------------------------------------
// MP-16a: background shell quartet.
//
// State lives in /tmp inside the Harbor container, since each tool.exec
// is a fresh shell call from our Rust process. Per-handle layout:
//
//   /tmp/kimetsu-bg-<handle>.meta      JSON {handle, pid, started_at_sec,
//                                            program, cwd}
//   /tmp/kimetsu-bg-<handle>.out       stdout drain
//   /tmp/kimetsu-bg-<handle>.err       stderr drain
//   /tmp/kimetsu-bg-<handle>.exitcode  written when the process exits
//
// Handles look like `bg-<epoch_ns>-<rand4>` and are opaque to the model.
// `setsid` detaches the spawn from the parent's process group so killing
// our own subprocess doesn't reap the background tree.

const BG_HANDLE_PREFIX: &str = "bg-";

fn validate_bg_handle(handle: &str) -> Result<(), String> {
    if !handle.starts_with(BG_HANDLE_PREFIX) {
        return Err(format!("invalid handle: expected `{BG_HANDLE_PREFIX}*`, got {handle:?}"));
    }
    if handle.len() > 128
        || handle
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return Err(format!("invalid handle characters: {handle:?}"));
    }
    Ok(())
}

fn harbor_shell_background(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(program) = input_str(input, "program") else {
        return json!({ "error": "shell_background requires `program`" });
    };
    let args: Vec<String> = input
        .get("args")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd_relative = input_str(input, "cwd_relative").map(str::to_string);

    // Build the shell-side command: write meta + spawn + record exit code.
    // We use bash because we need $! / wait / setsid. The composed script
    // returns the handle on stdout for the model to capture.
    let mut argv = String::from(shell_quote(program));
    for a in &args {
        argv.push(' ');
        argv.push_str(&shell_quote(a));
    }
    let script = format!(
        r#"
set -e
HANDLE="{prefix}$(date +%s%N)-$(printf '%04x' $((RANDOM * RANDOM % 65536)))"
BASE="/tmp/kimetsu-bg-$HANDLE"
META="$BASE.meta"
OUT="$BASE.out"
ERR="$BASE.err"
EXIT="$BASE.exitcode"
: > "$OUT"
: > "$ERR"
rm -f "$EXIT"
( setsid bash -c '{argv}; echo $? > '"$EXIT" \
    > "$OUT" 2> "$ERR" < /dev/null ) &
PID=$!
disown
printf '{{"handle":"%s","pid":%s,"started_at_sec":%s,"program":%s,"cwd":%s}}\n' \
    "$HANDLE" "$PID" "$(date +%s)" {prog_json} {cwd_json} > "$META"
echo "$HANDLE $PID"
"#,
        prefix = BG_HANDLE_PREFIX,
        argv = argv.replace('\'', "'\\''"),
        prog_json = shell_quote(&format!("\"{program}\"")),
        cwd_json = shell_quote(&format!(
            "\"{}\"",
            cwd_relative.as_deref().unwrap_or(".")
        )),
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        cwd_relative.clone(),
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_background shell failed: {e}") }),
    };
    if out.exit_code != 0 {
        return json!({
            "error": format!("shell_background launch failed: exit {}; stderr {}", out.exit_code, truncate(&out.stderr_summary, 400))
        });
    }
    // Parse "HANDLE PID\n"
    let first_line = out.stdout_summary.lines().next().unwrap_or("").trim();
    let mut parts = first_line.splitn(2, ' ');
    let handle = parts.next().unwrap_or("").to_string();
    let pid: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    if handle.is_empty() || pid == 0 {
        return json!({
            "error": format!("shell_background: could not parse launcher output: {first_line:?}")
        });
    }
    json!({
        "handle": handle,
        "pid": pid,
        "program": program,
        "started": true,
        "stdout_path": format!("/tmp/kimetsu-bg-{handle}.out"),
        "stderr_path": format!("/tmp/kimetsu-bg-{handle}.err"),
    })
}

fn harbor_shell_status(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(handle) = input_str(input, "handle") else {
        return json!({ "error": "shell_status requires `handle`" });
    };
    if let Err(e) = validate_bg_handle(handle) {
        return json!({ "error": e });
    }
    let script = format!(
        r#"
BASE=/tmp/kimetsu-bg-{handle}
META="$BASE.meta"
OUT="$BASE.out"
ERR="$BASE.err"
EXIT="$BASE.exitcode"
if [[ ! -f "$META" ]]; then
    echo "no_such_handle"
    exit 0
fi
PID=$(grep -oE '"pid":[0-9]+' "$META" | head -1 | cut -d: -f2)
STARTED=$(grep -oE '"started_at_sec":[0-9]+' "$META" | head -1 | cut -d: -f2)
NOW=$(date +%s)
RUNTIME=$((NOW - STARTED))
BYTES_OUT=$(wc -c < "$OUT" 2>/dev/null || echo 0)
BYTES_ERR=$(wc -c < "$ERR" 2>/dev/null || echo 0)
if kill -0 "$PID" 2>/dev/null; then
    echo "running pid=$PID runtime=$RUNTIME out=$BYTES_OUT err=$BYTES_ERR"
elif [[ -f "$EXIT" ]]; then
    EC=$(cat "$EXIT")
    echo "exited pid=$PID runtime=$RUNTIME exit=$EC out=$BYTES_OUT err=$BYTES_ERR"
else
    echo "gone pid=$PID runtime=$RUNTIME out=$BYTES_OUT err=$BYTES_ERR"
fi
"#,
        handle = handle
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_status shell failed: {e}") }),
    };
    let line = out.stdout_summary.lines().next().unwrap_or("").trim().to_string();
    if line == "no_such_handle" {
        return json!({ "error": format!("no such handle: {handle}") });
    }
    parse_bg_status_line(&line, handle)
}

fn parse_bg_status_line(line: &str, handle: &str) -> Value {
    let mut state = "unknown";
    let mut fields = std::collections::HashMap::new();
    for (i, tok) in line.split_whitespace().enumerate() {
        if i == 0 {
            state = tok;
            continue;
        }
        if let Some((k, v)) = tok.split_once('=') {
            fields.insert(k.to_string(), v.to_string());
        }
    }
    let pid: u32 = fields
        .get("pid")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let runtime: u64 = fields
        .get("runtime")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bytes_out: u64 = fields
        .get("out")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bytes_err: u64 = fields
        .get("err")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let exit_code: Option<i32> = fields.get("exit").and_then(|s| s.parse().ok());
    let running = state == "running";
    json!({
        "handle": handle,
        "running": running,
        "state": state,
        "pid": pid,
        "runtime_sec": runtime,
        "exit_code": exit_code,
        "bytes_stdout": bytes_out,
        "bytes_stderr": bytes_err,
    })
}

fn harbor_shell_output(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(handle) = input_str(input, "handle") else {
        return json!({ "error": "shell_output requires `handle`" });
    };
    if let Err(e) = validate_bg_handle(handle) {
        return json!({ "error": e });
    }
    let tail_bytes = input_u32(input, "tail_bytes", 8192).max(1);
    let script = format!(
        r#"
BASE=/tmp/kimetsu-bg-{handle}
OUT="$BASE.out"
ERR="$BASE.err"
if [[ ! -f "$BASE.meta" ]]; then
    echo "no_such_handle"
    exit 0
fi
echo "::STDOUT::"
tail -c {n} "$OUT" 2>/dev/null || true
echo "::STDERR::"
tail -c {n} "$ERR" 2>/dev/null || true
echo "::END::"
"#,
        handle = handle,
        n = tail_bytes
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_output shell failed: {e}") }),
    };
    let raw = out.stdout_summary;
    if raw.contains("no_such_handle") {
        return json!({ "error": format!("no such handle: {handle}") });
    }
    let stdout_tail = extract_between(&raw, "::STDOUT::", "::STDERR::");
    let stderr_tail = extract_between(&raw, "::STDERR::", "::END::");
    json!({
        "handle": handle,
        "stdout_tail": stdout_tail,
        "stderr_tail": stderr_tail,
        "tail_bytes": tail_bytes,
    })
}

fn extract_between(raw: &str, start: &str, end: &str) -> String {
    if let Some(s) = raw.find(start) {
        let after = &raw[s + start.len()..];
        if let Some(e) = after.find(end) {
            return after[..e].trim_matches('\n').to_string();
        }
        return after.trim_matches('\n').to_string();
    }
    String::new()
}

fn harbor_shell_stop(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(handle) = input_str(input, "handle") else {
        return json!({ "error": "shell_stop requires `handle`" });
    };
    if let Err(e) = validate_bg_handle(handle) {
        return json!({ "error": e });
    }
    let signal = input_u32(input, "signal", 15).clamp(1, 31);
    let script = format!(
        r#"
BASE=/tmp/kimetsu-bg-{handle}
META="$BASE.meta"
EXIT="$BASE.exitcode"
if [[ ! -f "$META" ]]; then
    echo "no_such_handle"
    exit 0
fi
PID=$(grep -oE '"pid":[0-9]+' "$META" | head -1 | cut -d: -f2)
STARTED=$(grep -oE '"started_at_sec":[0-9]+' "$META" | head -1 | cut -d: -f2)
NOW=$(date +%s)
RUNTIME=$((NOW - STARTED))
if kill -0 "$PID" 2>/dev/null; then
    # Target the whole process group (setsid'd at spawn time).
    PGID=$(ps -o pgid= -p "$PID" 2>/dev/null | tr -d ' ' || true)
    if [[ -n "$PGID" ]]; then
        kill -{signal} -"$PGID" 2>/dev/null || kill -{signal} "$PID" 2>/dev/null || true
    else
        kill -{signal} "$PID" 2>/dev/null || true
    fi
    # Wait briefly for it to die.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        kill -0 "$PID" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$PID" 2>/dev/null; then
        echo "still_running pid=$PID runtime=$RUNTIME signal={signal}"
    elif [[ -f "$EXIT" ]]; then
        echo "stopped pid=$PID runtime=$RUNTIME exit=$(cat "$EXIT")"
    else
        echo "stopped pid=$PID runtime=$RUNTIME exit=-{signal}"
    fi
elif [[ -f "$EXIT" ]]; then
    echo "already_exited pid=$PID runtime=$RUNTIME exit=$(cat "$EXIT")"
else
    echo "gone pid=$PID runtime=$RUNTIME"
fi
"#,
        handle = handle,
        signal = signal
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("shell_stop shell failed: {e}") }),
    };
    let line = out.stdout_summary.lines().next().unwrap_or("").trim();
    if line == "no_such_handle" {
        return json!({ "error": format!("no such handle: {handle}") });
    }
    let status = parse_bg_status_line(line, handle);
    let stopped = matches!(
        status.get("state").and_then(Value::as_str),
        Some("stopped") | Some("already_exited") | Some("gone")
    );
    let mut obj = status.as_object().cloned().unwrap_or_default();
    obj.insert("stopped".to_string(), json!(stopped));
    obj.insert("signal".to_string(), json!(signal));
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// MP-16c: view_image — metadata + optional base64 for workspace images.
//
// Cheap version: shell out to `file`, `wc -c`, `sha256sum`, plus a sniff
// for image dimensions when ImageMagick's `identify` is present. We
// deliberately don't pipe the image to Claude's vision API here —
// surfacing image bytes to the model via the claude `-p` text channel
// would require provider-level work outside MP-16 scope. The metadata
// + optional base64 still help: the model can verify expected files,
// compare hashes, or hand the base64 to a Python script it writes.

const VIEW_IMAGE_DEFAULT_MAX_BYTES: u32 = 256 * 1024;

fn harbor_view_image(runtime: &mut crate::tools::ToolRuntime, input: &Value) -> Value {
    let Some(path) = input_str(input, "path") else {
        return json!({ "error": "view_image requires `path`" });
    };
    let include_b64 = input
        .get("include_base64")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_bytes = input_u32(input, "max_bytes", VIEW_IMAGE_DEFAULT_MAX_BYTES).max(64);

    let want_b64 = if include_b64 { "1" } else { "0" };
    let script = format!(
        r#"
set -u
P={path_q}
if [[ ! -f "$P" ]]; then
    echo "::ERR::not a regular file"
    exit 0
fi
SIZE=$(wc -c < "$P" 2>/dev/null || echo 0)
SHA=$(sha256sum "$P" 2>/dev/null | awk '{{print $1}}')
TYPE=$(file -b --mime-type "$P" 2>/dev/null || echo "application/octet-stream")
WH=$(identify -format '%w %h' "$P" 2>/dev/null | head -1 || echo "")
echo "::SIZE::$SIZE"
echo "::SHA::$SHA"
echo "::TYPE::$TYPE"
echo "::WH::$WH"
if [[ "{want_b64}" == "1" && "$SIZE" -le "{max_bytes}" ]]; then
    echo "::B64::"
    base64 -w0 "$P" 2>/dev/null
    echo
    echo "::B64END::"
fi
"#,
        path_q = shell_quote(path),
        want_b64 = want_b64,
        max_bytes = max_bytes,
    );
    let out = match run_shell(
        runtime,
        "bash",
        vec!["-c".into(), script],
        None,
        Some(TOOL_DEFAULT_TIMEOUT_SECS),
    ) {
        Ok(o) => o,
        Err(e) => return json!({ "error": format!("view_image shell failed: {e}") }),
    };
    let raw = &out.stdout_summary;
    if let Some(err) = raw.strip_prefix("::ERR::") {
        return json!({
            "error": format!("view_image: {}", err.lines().next().unwrap_or("").trim()),
            "path": path,
        });
    }
    let size = extract_marker(raw, "::SIZE::")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let sha = extract_marker(raw, "::SHA::").unwrap_or_default();
    let mime = extract_marker(raw, "::TYPE::").unwrap_or_default();
    let wh = extract_marker(raw, "::WH::").unwrap_or_default();
    let (width, height) = parse_wh(&wh);
    let base64 = if include_b64 {
        Some(extract_between(raw, "::B64::", "::B64END::").replace('\n', ""))
    } else {
        None
    };
    let format = mime
        .strip_prefix("image/")
        .map(str::to_string)
        .unwrap_or(mime.clone());
    json!({
        "path": path,
        "format": format,
        "mime": mime,
        "size_bytes": size,
        "sha256": sha,
        "width": width,
        "height": height,
        "base64": base64,
        "base64_truncated": include_b64 && size > max_bytes as u64,
    })
}

fn extract_marker(raw: &str, marker: &str) -> Option<String> {
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn parse_wh(s: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = s.split_whitespace();
    let w = parts.next().and_then(|p| p.parse().ok());
    let h = parts.next().and_then(|p| p.parse().ok());
    (w, h)
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

    // ----- MP-14e unit tests (pure-Rust paths, no shell needed) -----

    #[test]
    fn plan_tool_validates_and_normalizes_todos() {
        let input = json!({
            "todos": [
                {"content": "scan", "status": "completed", "activeForm": "Scanning"},
                {"content": "fix",  "status": "in_progress"},
                {"content": "ship", "status": "pending"},
            ]
        });
        let result = harbor_plan(&input);
        assert_eq!(result["total"], 3);
        assert_eq!(result["pending"], 1);
        assert_eq!(result["in_progress"], 1);
        assert_eq!(result["completed"], 1);
        // activeForm defaults to content when omitted.
        assert_eq!(result["todos"][1]["activeForm"], "fix");
        assert_eq!(result["todos"][0]["activeForm"], "Scanning");
    }

    #[test]
    fn plan_tool_rejects_invalid_status() {
        let input = json!({
            "todos": [
                {"content": "x", "status": "blocked"},
            ]
        });
        let result = harbor_plan(&input);
        let err = result["error"].as_str().unwrap_or("");
        assert!(err.contains("must be pending|in_progress|completed"), "got: {err}");
    }

    #[test]
    fn plan_tool_rejects_missing_fields() {
        assert!(harbor_plan(&json!({})).get("error").is_some());
        assert!(harbor_plan(&json!({"todos": [{"status": "pending"}]}))
            .get("error")
            .is_some());
        assert!(harbor_plan(&json!({"todos": [{"content": "x"}]}))
            .get("error")
            .is_some());
    }

    #[test]
    fn think_tool_acknowledges_and_reports_length() {
        let thought = "consider whether to rebuild";
        let result = harbor_think(&json!({"thought": thought}));
        assert_eq!(result["ack"], true);
        assert_eq!(result["thought_len"], thought.len() as u64);
        assert!(harbor_think(&json!({})).get("error").is_some());
    }

    #[test]
    fn check_workspace_path_rejects_escape_attempts() {
        assert!(check_workspace_path("p", "").is_err());
        assert!(check_workspace_path("p", "/etc/passwd").is_err());
        assert!(check_workspace_path("p", "../sibling").is_err());
        assert!(check_workspace_path("p", "ok/path/../bad").is_err());
        assert!(check_workspace_path("p", "ok/path").is_ok());
        assert!(check_workspace_path("p", "deep/nested/file.rs").is_ok());
    }

    // ----- MP-16 unit tests (pure-Rust paths) -----

    #[test]
    fn codex_patch_translator_handles_update_file() {
        let codex = "\
*** Begin Patch
*** Update File: src/lib.rs
@@
-old line
+new line
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("--- a/src/lib.rs"));
        assert!(unified.contains("+++ b/src/lib.rs"));
        assert!(unified.contains("-old line"));
        assert!(unified.contains("+new line"));
    }

    #[test]
    fn codex_patch_translator_handles_add_file() {
        let codex = "\
*** Begin Patch
*** Add File: notes/new.md
+# Heading
+body line
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("--- /dev/null"));
        assert!(unified.contains("+++ b/notes/new.md"));
        // Hunk header reflects 2 added lines.
        assert!(unified.contains("@@ -0,0 +1,2 @@"));
        assert!(unified.contains("+# Heading"));
    }

    #[test]
    fn codex_patch_translator_handles_delete_file() {
        let codex = "\
*** Begin Patch
*** Delete File: legacy/old.rs
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("--- a/legacy/old.rs"));
        assert!(unified.contains("+++ /dev/null"));
    }

    #[test]
    fn codex_patch_translator_synthesizes_hunk_when_missing() {
        // Update File with no `@@` marker still gets a synthesized header.
        let codex = "\
*** Begin Patch
*** Update File: src/x.rs
-removed
+added
*** End Patch
";
        let unified = translate_codex_patch(codex).expect("translate");
        assert!(unified.contains("@@ -1,"));
    }

    #[test]
    fn codex_patch_translator_rejects_unmarked_input() {
        let plain = "diff --git a/x b/x\n--- a/x\n+++ b/x\n";
        let err = translate_codex_patch(plain).unwrap_err();
        assert!(err.contains("Begin Patch"));
    }

    #[test]
    fn validate_bg_handle_accepts_well_formed() {
        assert!(validate_bg_handle("bg-1234567890-abcd").is_ok());
        assert!(validate_bg_handle("bg-abc_def-0001").is_ok());
    }

    #[test]
    fn validate_bg_handle_rejects_garbage() {
        assert!(validate_bg_handle("").is_err());
        assert!(validate_bg_handle("noprefix").is_err());
        assert!(validate_bg_handle("bg-with;semicolon").is_err());
        assert!(validate_bg_handle("bg-`backtick`").is_err());
        // Pathological-length handle is refused.
        let oversized = format!("bg-{}", "a".repeat(200));
        assert!(validate_bg_handle(&oversized).is_err());
    }

    #[test]
    fn parse_bg_status_line_running_state() {
        let v = parse_bg_status_line(
            "running pid=12345 runtime=42 out=1024 err=128",
            "bg-1-abcd",
        );
        assert_eq!(v["state"], "running");
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 12345);
        assert_eq!(v["runtime_sec"], 42);
        assert_eq!(v["bytes_stdout"], 1024);
        assert_eq!(v["bytes_stderr"], 128);
        assert!(v["exit_code"].is_null());
    }

    #[test]
    fn parse_bg_status_line_exited_state() {
        let v = parse_bg_status_line(
            "exited pid=999 runtime=120 exit=0 out=2048 err=0",
            "bg-2-beef",
        );
        assert_eq!(v["state"], "exited");
        assert_eq!(v["running"], false);
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn extract_between_pulls_section() {
        let raw = "::A::\nhello\nworld\n::B::\ntrailing\n";
        assert_eq!(extract_between(raw, "::A::", "::B::"), "hello\nworld");
    }

    #[test]
    fn extract_marker_finds_first_match() {
        let raw = "::SIZE::1234\n::SHA::abc\n";
        assert_eq!(extract_marker(raw, "::SIZE::"), Some("1234".to_string()));
        assert_eq!(extract_marker(raw, "::MISSING::"), None);
    }

    #[test]
    fn parse_wh_handles_well_formed_and_garbage() {
        assert_eq!(parse_wh("640 480"), (Some(640), Some(480)));
        assert_eq!(parse_wh(""), (None, None));
        assert_eq!(parse_wh("abc def"), (None, None));
    }

    // ----- MP-17 #10: unified-diff parser tests -----

    #[test]
    fn parse_unified_diff_single_file_single_hunk() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/lib.rs");
        assert!(!files[0].is_new);
        assert!(!files[0].is_delete);
        assert_eq!(files[0].hunks.len(), 1);
        let h = &files[0].hunks[0];
        assert!(h.old.contains("let x = 1;"));
        assert!(h.new.contains("let x = 2;"));
        assert!(h.old.contains("fn main()"));
        assert!(h.new.contains("fn main()"));
    }

    #[test]
    fn parse_unified_diff_detects_new_file() {
        let diff = "\
--- /dev/null
+++ b/notes/new.md
@@ -0,0 +1,2 @@
+# heading
+body
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "notes/new.md");
        assert!(files[0].is_new);
        assert!(!files[0].is_delete);
        assert_eq!(files[0].hunks.len(), 1);
        assert!(files[0].hunks[0].new.contains("# heading"));
    }

    #[test]
    fn parse_unified_diff_detects_delete_file() {
        let diff = "\
--- a/old.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-line
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "old.rs");
        assert!(!files[0].is_new);
        assert!(files[0].is_delete);
    }

    #[test]
    fn parse_unified_diff_multi_file() {
        let diff = "\
--- a/x.rs
+++ b/x.rs
@@ -1,1 +1,1 @@
-a
+b
--- a/y.rs
+++ b/y.rs
@@ -1,1 +1,1 @@
-c
+d
";
        let files = parse_unified_diff(diff).expect("parse");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "x.rs");
        assert_eq!(files[1].path, "y.rs");
    }

    #[test]
    fn parse_unified_diff_rejects_missing_plus_header() {
        let diff = "--- a/x\nbogus\n";
        assert!(parse_unified_diff(diff).is_err());
    }

    #[test]
    fn parse_unified_diff_rejects_empty() {
        assert!(parse_unified_diff("").is_err());
        assert!(parse_unified_diff("no headers here").is_err());
    }

    #[test]
    fn extract_diff_path_strips_ab_prefix() {
        assert_eq!(extract_diff_path("a/src/lib.rs"), "src/lib.rs");
        assert_eq!(extract_diff_path("b/notes/new.md"), "notes/new.md");
        assert_eq!(extract_diff_path("plain/path.rs"), "plain/path.rs");
        assert_eq!(extract_diff_path("  a/x.rs  "), "x.rs");
    }
}
