use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::ids::new_id;
use serde::Deserialize;

use crate::agent_loop::extract_json_object;
use crate::model::{
    MessageContent, MessageRole, ModelProvider, ModelRequest, ModelResponse, StopReason, TokenUsage,
    ToolCall, ToolDefinition,
};

#[derive(Debug, Clone)]
pub struct ClaudeCodeProvider {
    api_key: String,
    model: String,
    timeout: Duration,
    // MP-15b: `--max-budget-usd` removed from the claude CLI invocation.
    // Claude Code's internal budget message ("Budget is $0/$5") was being
    // misread by the model on `make-mips-interpreter` — it interpreted the
    // "spent/total" notation as "remaining/total" and prematurely bailed
    // after 7 tool calls / 1m35s. The outer kimetsu loop (turn budget +
    // run-level max_total_cost_usd) is the real budget gate; passing
    // --max-budget-usd to claude was redundant and harmful. The field is
    // gone; if a future iteration needs an inner-loop cap, route it through
    // a separate (less ambiguous) mechanism.
    //
    // v0.3.4b: stable tempdir per provider (was: fresh per call).
    //
    // Before this change, every `complete()` invocation created a brand-
    // new `TempCommandDir` under `$TEMP/kimetsu-claude-code-<run_id>/`
    // and pointed claude's `--current-dir` + `CLAUDE_CONFIG_DIR` at it.
    // Per-call tempdir means:
    //   * claude treats each call as a fresh "project" — its prompt-cache
    //     keys (which include cwd-derived state) miss, so we pay
    //     `cache_creation` freight on the system prompt every turn.
    //   * the directory churn shows up as a perf cliff on Windows where
    //     mkdir + rmdir aren't free even for empty dirs.
    //
    // Lifting it to a provider-scoped field means a single chat session
    // (or harbor session, or pipeline run) sees one stable cwd across
    // every `complete()` call. The `Arc` wrapper keeps the `Clone` derive
    // sound — cloning ClaudeCodeProvider now shares the tempdir instead
    // of double-dropping it.
    //
    // Drop on `TempCommandDir` deletes the dir when the last Arc drops,
    // so we don't leak across sessions.
    work_dir: Arc<TempCommandDir>,
}

impl ClaudeCodeProvider {
    pub fn from_config(repo_root: &Path, config: &ProjectConfig) -> KimetsuResult<Option<Self>> {
        Self::from_config_with_key(repo_root, config, None)
    }

    pub fn from_config_with_key(
        repo_root: &Path,
        config: &ProjectConfig,
        key_override: Option<&str>,
    ) -> KimetsuResult<Option<Self>> {
        if config.model.provider != "claude_code" {
            return Err(format!("unsupported model provider: {}", config.model.provider).into());
        }

        let secret = key_override
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| resolve_env_value(repo_root, &config.model.api_key_env));
        let Some(secret) = secret else {
            return Ok(None);
        };

        // v0.3.4b: provision the stable tempdir + config subdir once at
        // construction. Both live for the provider's lifetime.
        let work_dir = TempCommandDir::create()?;
        let config_dir = work_dir.path().join("config");
        fs::create_dir_all(&config_dir)?;

        Ok(Some(Self {
            api_key: secret,
            model: config.model.model.clone(),
            timeout: Duration::from_secs(config.model.request_timeout_secs),
            work_dir: Arc::new(work_dir),
        }))
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
}

impl ModelProvider for ClaudeCodeProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        let tool_loop = !request.tools.is_empty();
        let (mut system_prompt, prompt) = render_request_for_claude_code(&request)?;
        if tool_loop {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&render_tool_protocol(&request.tools));
        }
        // v0.3.4b: reuse the provider's stable tempdir. Created once in
        // from_config_with_key; survives until the provider is dropped.
        // Stable cwd keeps Anthropic's prompt-cache keys aligned across
        // calls so cache_read_input_tokens has a chance of growing.
        let work_dir: &TempCommandDir = &self.work_dir;
        let config_dir = work_dir.path().join("config");
        // Defensive: re-create config dir if something cleared it
        // between calls (e.g. user blew away $TEMP). Cheap no-op if it
        // already exists.
        fs::create_dir_all(&config_dir)?;

        // MP-13f: retry loop for transient Anthropic 5xx errors. The
        // MP-13 brain gauntlet caught a real 14-of-16-trials wipeout
        // when Anthropic returned 529 Overloaded across a ~70-minute
        // window; the kimetsu binary aborted on the first transient
        // failure and the adapter saw the process exit before
        // agent.done. We now retry up to 3 times with 5s/15s
        // exponential backoff. Outer kimetsu loop is still the budget
        // ceiling.
        const MAX_ATTEMPTS: u32 = 3;
        const BASE_BACKOFF_SECS: u64 = 5;

        // MP-17g: pipe the prompt via stdin instead of as a positional argv.
        //
        // Background: after MP-17a's expanded user message (heuristics +
        // worked example + pitfalls) + brain context + accumulated multi-
        // turn message history, the rendered prompt routinely crosses
        // 50-100KB by turn 10. Passing it as a single argv slot to the
        // claude CLI triggered `exec()` -> E2BIG ("Argument list too long",
        // os error 7) on the longer tasks (make-mips-interpreter,
        // path-tracing, etc.) — observed during the MP-17 validation
        // gauntlet attempt at 19:16Z. Linux's ARG_MAX is typically 128KB
        // shared between argv + envp; once argv approaches that ceiling
        // any further messages cause exec to fail outright.
        //
        // The fix: drop the positional prompt argument and write the
        // prompt to the child's stdin, which has no comparable size limit
        // for `claude -p` mode. We also drop --tools "" because the
        // system-prompt + user-message envelope grammar already overrides
        // tool advertising; the empty --tools arg was a belt-and-braces
        // measure that combined with our long prompt could push us past
        // the limit on some shells.
        let build_command = || {
            let mut command = Command::new("claude");
            command
                .current_dir(work_dir.path())
                .env("CLAUDE_CONFIG_DIR", &config_dir)
                .env("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1")
                // Route the configured token through the OAuth env var; the local
                // `claude` CLI accepts OAuth for non-`--bare` invocations.
                .env("CLAUDE_CODE_OAUTH_TOKEN", &self.api_key)
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("ANTHROPIC_AUTH_TOKEN")
                .arg("-p")
                // MP-17g: no positional prompt; we feed it via stdin.
                .arg("--input-format")
                .arg("text")
                // MP-17k: switch from `json` (buffered single blob) to
                // `stream-json` (newline-delimited events). With `json`,
                // claude held the entire response in memory until done
                // and our MP-17f idle-byte heartbeat saw 0 bytes for the
                // whole 1500s on hard tasks (circuit-fibsqrt was the
                // canonical hang). stream-json emits a `system` init
                // event immediately, then `assistant` events as the model
                // streams tokens, then a final `result` event with the
                // usage/cost summary. Our heartbeat sees continuous
                // activity; we extract the `result` event for the same
                // shape parse_claude_code_output used to consume.
                .arg("--output-format")
                .arg("stream-json")
                // MP-17k addendum: claude CLI REQUIRES --verbose when
                // pairing -p / --print with --output-format=stream-json.
                // Without it the CLI errors out at startup with
                // "When using --print, --output-format=stream-json
                // requires --verbose". Doesn't affect our parser —
                // extract_result_event walks the events regardless of
                // how many or what type.
                .arg("--verbose")
                .arg("--model")
                .arg(&self.model)
                // MP-7d / MP-13c: bump to 16. 8 was already a workaround for
                // Claude Code's inner-loop interpretation of our envelope
                // tool_use. Some MP-12 trials (compile-compcert,
                // circuit-fibsqrt) showed CC's inner loop running close to
                // 8 turns before our envelope grammar finally fired; the
                // result was claude_code provider stalls. 16 doubles the
                // headroom for those cases without exploding cost — the
                // outer kimetsu loop is still the real budget gate.
                .arg("--max-turns")
                .arg("16")
                // MP-15b: `--max-budget-usd` removed. Claude Code surfaces it
                // as "Budget is $X/$Y", which the model misreads as
                // remaining/total instead of spent/total — it bailed
                // prematurely on `make-mips-interpreter` after 1m35s
                // ("I have no budget to spend on this task"). The outer
                // kimetsu loop already enforces budget via cost tracking
                // and turn count; the inner cap was redundant.
                .arg("--no-session-persistence")
                // MP-17h: keep `--tools ""` to disable Claude Code's
                // built-in tool surface (Bash, Edit, Read, Write, Glob,
                // Grep, TodoWrite, Task, ...). Without this, claude runs
                // its own inner agentic loop using those tools, which:
                //   (a) conflicts with our envelope grammar — model
                //       gets two tool surfaces and chooses wrong;
                //   (b) buffers entire interaction with `--output-format
                //       json` → our heartbeat sees 0 bytes for the full
                //       1500s idle window and kills the subprocess;
                //   (c) routes filesystem ops to the wrong place
                //       (claude's cwd, not the Harbor container).
                // MP-17g dropped this flag thinking it was redundant
                // with our envelope override; it isn't. The flag does
                // NOT affect our argv ARG_MAX situation (stdin-pipe
                // already handles that).
                .arg("--tools")
                .arg("")
                .arg("--permission-mode")
                .arg("bypassPermissions")
                .arg("--system-prompt")
                .arg(&system_prompt);
            command
        };

        let mut last_error: Option<String> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let output = run_with_timeout_stdin(build_command(), &prompt, self.timeout).map_err(|err| {
                redact_token(
                    &format!("claude_code provider failed: {err}"),
                    &self.api_key,
                )
            })?;

            // Two retry conditions:
            //  (a) Claude Code exits non-zero AND stdout JSON has
            //      api_error_status >= 500
            //  (b) stdout text contains "Overloaded" as a fallback
            //      sentinel for cases where the JSON parse fails
            let stdout_text = String::from_utf8_lossy(&output.stdout);
            let transient = if !output.status.success() {
                // MP-17k: stdout is now stream-json (one JSON event per
                // line). Extract the final `result` event and check its
                // api_error_status.
                let cc: Option<ClaudeCodeJson> = extract_result_event(&stdout_text).ok();
                let status_5xx = cc
                    .as_ref()
                    .and_then(|c| c.api_error_status)
                    .map(|s| s >= 500)
                    .unwrap_or(false);
                let overloaded_text = stdout_text.contains("Overloaded")
                    || stdout_text.contains("\"api_error_status\":5");
                status_5xx || overloaded_text
            } else {
                false
            };

            if !output.status.success() {
                let err_msg = redact_token(
                    &format!(
                        "claude_code provider failed (exit {}, attempt {attempt}/{MAX_ATTEMPTS}): {}",
                        output
                            .status
                            .code()
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "signal".to_string()),
                        output_summary(&output)
                    ),
                    &self.api_key,
                );
                if transient && attempt < MAX_ATTEMPTS {
                    let backoff = BASE_BACKOFF_SECS * (1u64 << (attempt - 1));
                    eprintln!(
                        "claude_code transient error on attempt {attempt}/{MAX_ATTEMPTS}; retrying in {backoff}s"
                    );
                    thread::sleep(Duration::from_secs(backoff));
                    last_error = Some(err_msg);
                    continue;
                }
                return Err(err_msg.into());
            }

            let mut response = parse_claude_code_output(&output.stdout)?;
            if tool_loop {
                apply_tool_envelope(&mut response);
            }
            return Ok(response);
        }

        Err(last_error
            .unwrap_or_else(|| "claude_code provider exhausted retries".to_string())
            .into())
    }
}

fn render_request_for_claude_code(request: &ModelRequest) -> KimetsuResult<(String, String)> {
    let mut system_parts = Vec::new();
    let mut prompt_parts = Vec::new();

    for message in &request.messages {
        let text = render_message_text(&message.content);
        if text.trim().is_empty() {
            continue;
        }

        match &message.role {
            MessageRole::System => system_parts.push(text),
            MessageRole::User => prompt_parts.push(text),
            MessageRole::Assistant => prompt_parts.push(format!("Assistant context:\n{text}")),
            MessageRole::Tool => {
                prompt_parts.push(format!("Tool result context:\n{text}"));
            }
        }
    }

    let system_prompt = if system_parts.is_empty() {
        "You are a text-only model provider inside Kimetsu. Follow the user request exactly."
            .to_string()
    } else {
        system_parts.join("\n\n")
    };
    let prompt = prompt_parts.join("\n\n");
    if prompt.trim().is_empty() {
        return Err("claude_code request has no prompt text".into());
    }
    Ok((system_prompt, prompt))
}

fn render_message_text(content: &[MessageContent]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block {
            MessageContent::Text { text } => parts.push(text.clone()),
            MessageContent::ToolCall { id, name, input } => {
                let payload = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                parts.push(format!(
                    "[previous tool_call id={id} name={name}] {payload}"
                ));
            }
            MessageContent::ToolResult {
                tool_call_id,
                name,
                output,
            } => {
                let payload = serde_json::to_string(output).unwrap_or_else(|_| "{}".to_string());
                parts.push(format!(
                    "[tool_result for call_id={tool_call_id} tool={name}] {payload}"
                ));
            }
        }
    }
    parts.join("\n")
}

fn render_tool_protocol(tools: &[ToolDefinition]) -> String {
    let mut catalog = String::new();
    for tool in tools {
        catalog.push_str(&format!("- {}: {}\n", tool.name, tool.description));
        let schema = serde_json::to_string(&tool.input_schema).unwrap_or_else(|_| "{}".to_string());
        catalog.push_str(&format!("  input_schema: {schema}\n"));
    }

    format!(
        "Tool-call protocol (Kimetsu executes tools on your behalf):\n\
         - When you need a tool, output exactly one JSON object and nothing else:\n\
           {{\"thought\": \"<short rationale>\", \"tool_call\": {{\"name\": \"<tool>\", \"input\": <object>}}}}\n\
         - To batch independent tools in one turn (no inter-call dependencies), use the parallel form:\n\
           {{\"thought\": \"<short rationale>\", \"tool_calls\": [{{\"name\": \"<tool>\", \"input\": <object>}}, ...]}}\n\
         - When you are done, output exactly one JSON object and nothing else:\n\
           {{\"thought\": \"<short rationale>\", \"finish\": {{\"summary\": \"<one-line outcome>\"}}}}\n\
         - No prose, no markdown, no backticks. One JSON object per response.\n\
         - Wait for all tool results before requesting more.\n\
         - Do not invent tool results. Do not output tool_result blocks yourself.\n\
         \n\
         Tool selection heuristics (MP-17a):\n\
         - Any command expected to take >60s (make / build / training /\n\
           ray-trace / large test suite) MUST use shell_background, not\n\
           shell_command. The wrapper kills foreground calls at ~1500s.\n\
           Poll with shell_status / shell_output between turns.\n\
         - Edits to existing files: edit_file (cheap, hash-checked) or\n\
           apply_patch (multi-file diff). NEVER use write_file to change\n\
           a few lines — it costs ~50x more tokens and risks corruption.\n\
         - Big-file reads: read_file with offset+limit, not full dumps.\n\
           multi_read when you need 2+ files at once.\n\
         - glob for path patterns, search_files for content grep. Pick\n\
           the cheaper one for the question you're asking.\n\
         - Use plan for >=3-step tasks; update statuses as you progress.\n\
         - Use think for reasoning slots; don't burn real tool calls on\n\
           pure deliberation.\n\
         - Batch independent calls via parallel tool_calls form to save\n\
           model round-trips.\n\
         \n\
         Available tools:\n{catalog}"
    )
}

fn apply_tool_envelope(response: &mut ModelResponse) {
    let Some(text) = response.text.as_deref() else {
        return;
    };
    let Some(json) = extract_json_object(text) else {
        return;
    };
    let envelope: ToolEnvelope = match serde_json::from_str(json) {
        Ok(envelope) => envelope,
        Err(_) => return,
    };

    // MP-14c: prefer the parallel-call form when both are present.
    // Either form turns into the same Vec<ToolCall> the agent loop
    // already iterates over.
    let parallel_calls: Vec<ToolCallPayload> = envelope.tool_calls.unwrap_or_default();
    let single_call: Option<ToolCallPayload> = envelope.tool_call;

    if !parallel_calls.is_empty() || single_call.is_some() {
        let mut calls: Vec<ToolCall> = parallel_calls
            .into_iter()
            .map(|c| ToolCall {
                id: format!("call_{}", new_id()),
                name: c.name,
                input: c.input,
            })
            .collect();
        if let Some(c) = single_call {
            calls.push(ToolCall {
                id: format!("call_{}", new_id()),
                name: c.name,
                input: c.input,
            });
        }
        response.tool_calls = calls;
        response.text = envelope.thought;
        response.stop_reason = StopReason::ToolUse;
    } else if envelope.finish.is_some() {
        response.text = Some(envelope.thought.unwrap_or_default());
        response.stop_reason = StopReason::EndTurn;
    }
}

#[derive(Debug, Default, Deserialize)]
struct ToolEnvelope {
    #[serde(default)]
    thought: Option<String>,
    #[serde(default)]
    tool_call: Option<ToolCallPayload>,
    /// MP-14c: parallel-call form. When the model emits multiple
    /// independent tool calls in one turn, it can pack them here
    /// instead of taking one turn per call. The agent loop executes
    /// them in order and feeds back all results at once.
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallPayload>>,
    #[serde(default)]
    finish: Option<FinishPayload>,
}

#[derive(Debug, Deserialize)]
struct ToolCallPayload {
    name: String,
    #[serde(default)]
    input: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct FinishPayload {
    #[allow(dead_code)]
    summary: Option<String>,
}

/// Spawn `command` with piped stdio and enforce a smart timeout.
///
/// Two-axis timeout (MP-17f):
///   - `timeout` is the IDLE deadline. If the subprocess produces no
///     bytes on stdout or stderr for this long, it's killed. An actively
///     producing process (compile, training, ray tracer streaming progress)
///     keeps resetting this timer and survives.
///   - A hard ceiling of `3 * timeout` is also enforced so a runaway chatty
///     subprocess can't loop indefinitely. Pick `timeout` such that
///     `3 * timeout` is still acceptable wall-clock for the hardest task
///     you expect (default 1500s -> hard cap 4500s = 75min).
///
/// Bug fix (observed during the MP-4 bench): on Windows the `claude` CLI
/// spawns Node worker grandchildren that inherit the parent's stdout/stderr
/// pipes. `TerminateProcess` on `claude` does NOT terminate the grandchildren
/// — they keep the write-end of the pipe open. A naive
/// `child.wait_with_output()` after `child.kill()` reads stdout/stderr until
/// EOF, and EOF never arrives because the grandchild still owns the
/// write-end. The whole bench parent then deadlocks reading from a pipe with
/// no writer that will ever close.
///
/// The fix: read stdout/stderr on dedicated drainer threads that increment
/// a shared atomic byte-counter as they read. The main loop polls the
/// counter to detect activity. On timeout we `child.kill()` + `child.wait()`
/// only and leak the drainers (they stay parked on a grandchild's pipe,
/// but that's a thread leak, not a deadlock).
fn run_with_timeout_stdin(
    mut command: Command,
    stdin_payload: &str,
    timeout: Duration,
) -> KimetsuResult<Output> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    // MP-17g: pipe the prompt via stdin to avoid argv ARG_MAX limits.
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();
    let hard_ceiling = timeout.saturating_mul(3);

    // Write the prompt to the child's stdin and close the pipe so the
    // claude CLI sees EOF and stops waiting for more input. We do this
    // BEFORE installing the drainer threads so we can surface a clear
    // "stdin write failed" error if the spawn was somehow bad.
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(err) = stdin.write_all(stdin_payload.as_bytes()) {
            // The child has died or refused stdin; kill and bail.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("stdin write failed: {err}").into());
        }
        // Dropping `stdin` here closes the pipe (EOF).
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_bytes_seen = Arc::new(AtomicU64::new(0));
    let stderr_bytes_seen = Arc::new(AtomicU64::new(0));

    let stdout_counter = stdout_bytes_seen.clone();
    let stdout_handle = stdout.map(move |mut s| {
        thread::spawn(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        stdout_counter.fetch_add(n as u64, AtomicOrdering::Relaxed);
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(buf)
        })
    });
    let stderr_counter = stderr_bytes_seen.clone();
    let stderr_handle = stderr.map(move |mut s| {
        thread::spawn(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        stderr_counter.fetch_add(n as u64, AtomicOrdering::Relaxed);
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(buf)
        })
    });

    let mut last_activity = Instant::now();
    let mut last_byte_total: u64 = 0;

    loop {
        if let Some(status) = child.try_wait()? {
            let stdout_bytes = stdout_handle
                .map(|h| h.join().unwrap_or_else(|_| Ok(Vec::new())).unwrap_or_default())
                .unwrap_or_default();
            let stderr_bytes = stderr_handle
                .map(|h| h.join().unwrap_or_else(|_| Ok(Vec::new())).unwrap_or_default())
                .unwrap_or_default();
            return Ok(Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            });
        }

        // Update last_activity if any bytes flowed since the last tick.
        let cur_total = stdout_bytes_seen.load(AtomicOrdering::Relaxed)
            + stderr_bytes_seen.load(AtomicOrdering::Relaxed);
        if cur_total > last_byte_total {
            last_activity = Instant::now();
            last_byte_total = cur_total;
        }

        let idle = last_activity.elapsed();
        let wall = started.elapsed();
        let idle_timeout = idle >= timeout;
        let hard_timeout = wall >= hard_ceiling;

        if idle_timeout || hard_timeout {
            let _ = child.kill();
            let _ = child.wait();
            let reason = if hard_timeout {
                format!(
                    "hard timeout: ran {}s (3x idle cap of {}s); child killed",
                    wall.as_secs(),
                    timeout.as_secs()
                )
            } else {
                format!(
                    "idle timeout: no stdio activity for {}s (after {}s total, {} bytes streamed); child killed",
                    idle.as_secs(),
                    wall.as_secs(),
                    last_byte_total
                )
            };
            return Err(reason.into());
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn parse_claude_code_output(stdout: &[u8]) -> KimetsuResult<ModelResponse> {
    let text = String::from_utf8_lossy(stdout);
    let response = extract_result_event(&text).map_err(|err| {
        format!(
            "failed to parse claude_code stream-json output: {err}; output: {}",
            truncate(&text, 700)
        )
    })?;

    if response.is_error.unwrap_or(false) || matches!(response.subtype.as_deref(), Some("error")) {
        return Err(format!(
            "claude_code returned an error result: {}",
            response
                .result
                .as_deref()
                .map(|value| truncate(value, 700))
                .unwrap_or_else(|| "missing result".to_string())
        )
        .into());
    }

    let result = response
        .result
        .ok_or("claude_code JSON output did not include result")?;
    let usage = response.usage.unwrap_or_default();

    Ok(ModelResponse {
        text: Some(result),
        tool_calls: Vec::new(),
        stop_reason: match response.stop_reason.as_deref() {
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            Some("end_turn") | None => StopReason::EndTurn,
            _ => StopReason::Error,
        },
        usage: TokenUsage {
            input_tokens: usage.input_tokens.unwrap_or_default(),
            output_tokens: usage.output_tokens.unwrap_or_default(),
            cost_usd: response.total_cost_usd.unwrap_or_default(),
            // v0.3.4a: pass cache stats through. Anthropic returns 0
            // (not absent) when no cache was used, so unwrap_or_default
            // matches the wire-level convention.
            cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or_default(),
            cache_read_input_tokens: usage.cache_read_input_tokens.unwrap_or_default(),
        },
    })
}

/// MP-17k: extract the final `result` event from claude's stream-json
/// output. With `--output-format stream-json` the CLI emits one JSON
/// object per line; the last event with `type: "result"` carries the
/// usage/cost summary in the same shape the old single-blob `json`
/// format used. Falls back to single-blob parse for backward compat /
/// for callers that still set `--output-format json`.
fn extract_result_event(text: &str) -> Result<ClaudeCodeJson, String> {
    // Walk lines; remember the last successfully-parsed `result` event.
    let mut last_result: Option<ClaudeCodeJson> = None;
    let mut parse_errors = 0u32;
    let mut seen_events = 0u32;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        seen_events += 1;
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                parse_errors += 1;
                continue;
            }
        };
        let event_type = value.get("type").and_then(|t| t.as_str());
        if event_type == Some("result") {
            match serde_json::from_value::<ClaudeCodeJson>(value) {
                Ok(parsed) => last_result = Some(parsed),
                Err(_) => parse_errors += 1,
            }
        }
    }
    if let Some(r) = last_result {
        return Ok(r);
    }
    // Backward compat: if no `result` event was found, try parsing the
    // whole text as a single-blob ClaudeCodeJson (old `--output-format
    // json` shape).
    if let Ok(blob) = serde_json::from_str::<ClaudeCodeJson>(text) {
        return Ok(blob);
    }
    if seen_events == 0 {
        Err("no JSON events on stdout (claude produced no output)".to_string())
    } else {
        Err(format!(
            "no `result` event in {seen_events} stream-json events (parse_errors={parse_errors})"
        ))
    }
}

fn output_summary(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = format!("stdout: {}; stderr: {}", stdout.trim(), stderr.trim());
    truncate(&summary, 700)
}

fn redact_token(value: &str, token: &str) -> String {
    if token.is_empty() {
        value.to_string()
    } else {
        value.replace(token, "[redacted]")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[derive(Debug, Deserialize)]
struct ClaudeCodeJson {
    subtype: Option<String>,
    is_error: Option<bool>,
    /// MP-13f: when CC's underlying Anthropic call returns a 5xx
    /// transient (typically 529 Overloaded during peak load), CC
    /// surfaces it here. We retry on >=500 with exponential backoff.
    #[serde(default)]
    api_error_status: Option<u32>,
    result: Option<String>,
    stop_reason: Option<String>,
    total_cost_usd: Option<f32>,
    usage: Option<ClaudeCodeUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeCodeUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    /// v0.3.4a: Anthropic prompt-cache write counter. The `result` event
    /// from claude's stream-json surfaces this when the API returns
    /// cache_creation_input_tokens; we forward it into `TokenUsage` so
    /// the chat REPL (and any future telemetry) can show cache hit/miss
    /// behavior per turn. Same for cache_read below.
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

#[derive(Debug)]
struct TempCommandDir {
    path: PathBuf,
}

impl TempCommandDir {
    fn create() -> KimetsuResult<Self> {
        let path = std::env::temp_dir().join(format!("kimetsu-claude-code-{}", new_id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempCommandDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelMessage, ToolChoice, ToolDefinition};
    use serde_json::json;

    #[test]
    fn parses_success_json() {
        let response = parse_claude_code_output(
            br#"{
                "subtype": "success",
                "is_error": false,
                "result": "{\"rationale\":\"ok\"}",
                "stop_reason": "end_turn",
                "total_cost_usd": 0.12,
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }"#,
        )
        .expect("parse response");

        assert_eq!(response.text.as_deref(), Some("{\"rationale\":\"ok\"}"));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
        assert_eq!(response.usage.cost_usd, 0.12);
    }

    #[test]
    fn renders_text_only_request() {
        let request = ModelRequest {
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![MessageContent::Text {
                        text: "Return JSON only.".to_string(),
                    }],
                },
                ModelMessage::user_text("Plan the patch."),
            ],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 1000,
            temperature: 0.2,
            metadata: json!(null),
        };

        let (system, prompt) = render_request_for_claude_code(&request).expect("render request");
        assert_eq!(system, "Return JSON only.");
        assert_eq!(prompt, "Plan the patch.");
    }

    #[test]
    fn renders_tool_protocol_when_tools_present() {
        let tools = vec![ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a UTF-8 text file inside the repo.".to_string(),
            input_schema: json!({ "type": "object" }),
        }];
        let protocol = render_tool_protocol(&tools);
        assert!(protocol.contains("Tool-call protocol"));
        assert!(protocol.contains("read_file"));
        assert!(protocol.contains("\"tool_call\""));
        assert!(protocol.contains("\"finish\""));
    }

    #[test]
    fn parses_tool_call_envelope_from_response_text() {
        let mut response = ModelResponse {
            text: Some(
                r#"{"thought":"need source","tool_call":{"name":"read_file","input":{"path":"src/lib.rs"}}}"#
                    .to_string(),
            ),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        apply_tool_envelope(&mut response);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(
            response.tool_calls[0].input,
            json!({ "path": "src/lib.rs" })
        );
        assert!(matches!(response.stop_reason, StopReason::ToolUse));
        assert_eq!(response.text.as_deref(), Some("need source"));
    }

    #[test]
    fn parses_finish_envelope_as_end_turn() {
        let mut response = ModelResponse {
            text: Some(
                r#"{"thought":"all done","finish":{"summary":"patched src/lib.rs"}}"#.to_string(),
            ),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        apply_tool_envelope(&mut response);
        assert!(response.tool_calls.is_empty());
        assert!(matches!(response.stop_reason, StopReason::EndTurn));
        assert_eq!(response.text.as_deref(), Some("all done"));
    }

    #[test]
    fn renders_tool_call_message_as_text_for_history() {
        let request = ModelRequest {
            messages: vec![
                ModelMessage::user_text("Patch the file."),
                ModelMessage::assistant_tool_calls(vec![ToolCall {
                    id: "call_42".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "src.txt"}),
                }]),
                ModelMessage::tool_result("call_42", "read_file", json!({"ok": true})),
            ],
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: 1000,
            temperature: 0.2,
            metadata: json!(null),
        };
        let (_, prompt) = render_request_for_claude_code(&request).expect("render");
        assert!(prompt.contains("previous tool_call"));
        assert!(prompt.contains("call_42"));
        assert!(prompt.contains("tool_result"));
    }

    // ----- MP-17k: stream-json parser tests -----

    #[test]
    fn stream_json_parser_returns_last_result_event() {
        let stream = r#"{"type":"system","subtype":"init","cwd":"/app"}
{"type":"assistant","message":{"role":"assistant","content":[]}}
{"type":"result","subtype":"success","is_error":false,"result":"hello","stop_reason":"end_turn","total_cost_usd":0.001,"usage":{"input_tokens":10,"output_tokens":5}}
"#;
        let parsed = extract_result_event(stream).expect("parse");
        assert_eq!(parsed.result.as_deref(), Some("hello"));
        assert_eq!(parsed.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(parsed.total_cost_usd, Some(0.001));
    }

    #[test]
    fn stream_json_parser_skips_non_result_events() {
        let stream = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"thinking..."}]}}
{"type":"user","message":{"role":"user","content":"tool result"}}
{"type":"result","subtype":"success","is_error":false,"result":"final","stop_reason":"end_turn"}
"#;
        let parsed = extract_result_event(stream).expect("parse");
        assert_eq!(parsed.result.as_deref(), Some("final"));
    }

    #[test]
    fn stream_json_parser_picks_LAST_result_when_multiple() {
        // Defensive: if claude emits multiple result events (e.g. due to
        // a bug or retry), we take the last one as the canonical wrap-up.
        let stream = r#"{"type":"result","is_error":true,"result":"transient error","stop_reason":"end_turn"}
{"type":"result","is_error":false,"result":"final answer","stop_reason":"end_turn"}
"#;
        let parsed = extract_result_event(stream).expect("parse");
        assert_eq!(parsed.result.as_deref(), Some("final answer"));
        assert_eq!(parsed.is_error, Some(false));
    }

    #[test]
    fn stream_json_parser_tolerates_malformed_lines() {
        let stream = r#"{"type":"system"}
this is not json
{"type":"result","is_error":false,"result":"survived","stop_reason":"end_turn"}
"#;
        let parsed = extract_result_event(stream).expect("parse");
        assert_eq!(parsed.result.as_deref(), Some("survived"));
    }

    #[test]
    fn stream_json_parser_falls_back_to_single_blob() {
        // Backward compat: if the caller still uses --output-format json,
        // we should still parse the single-blob payload.
        let blob = r#"{"subtype":"success","is_error":false,"result":"single-blob","stop_reason":"end_turn","total_cost_usd":0.002}"#;
        let parsed = extract_result_event(blob).expect("parse");
        assert_eq!(parsed.result.as_deref(), Some("single-blob"));
    }

    #[test]
    fn stream_json_parser_errors_when_no_result_event() {
        let stream = r#"{"type":"system"}
{"type":"assistant","message":{}}
"#;
        let err = extract_result_event(stream).unwrap_err();
        assert!(err.contains("no `result` event") || err.contains("no JSON events"));
    }

    #[test]
    fn stream_json_parser_errors_on_empty_output() {
        let err = extract_result_event("").unwrap_err();
        assert!(err.contains("no JSON events"));
    }
}
