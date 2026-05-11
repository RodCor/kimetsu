use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
    max_budget_usd: f32,
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

        Ok(Some(Self {
            api_key: secret,
            model: config.model.model.clone(),
            timeout: Duration::from_secs(config.model.request_timeout_secs),
            max_budget_usd: config.run.max_total_cost_usd,
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
        let work_dir = TempCommandDir::create()?;
        let config_dir = work_dir.path().join("config");
        fs::create_dir_all(&config_dir)?;

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
            .arg(&prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg(&self.model)
            .arg("--max-turns")
            .arg("1")
            .arg("--max-budget-usd")
            .arg(format!("{:.4}", self.max_budget_usd))
            .arg("--no-session-persistence")
            .arg("--tools")
            .arg("")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--system-prompt")
            .arg(&system_prompt);

        let output = run_with_timeout(command, self.timeout).map_err(|err| {
            redact_token(
                &format!("claude_code provider failed: {err}"),
                &self.api_key,
            )
        })?;
        if !output.status.success() {
            return Err(redact_token(
                &format!(
                    "claude_code provider failed (exit {}): {}",
                    output
                        .status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_string()),
                    output_summary(&output)
                ),
                &self.api_key,
            )
            .into());
        }

        let mut response = parse_claude_code_output(&output.stdout)?;
        if tool_loop {
            apply_tool_envelope(&mut response);
        }
        Ok(response)
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
         - When you are done, output exactly one JSON object and nothing else:\n\
           {{\"thought\": \"<short rationale>\", \"finish\": {{\"summary\": \"<one-line outcome>\"}}}}\n\
         - No prose, no markdown, no backticks. One JSON object per response.\n\
         - You may call exactly one tool per response. Wait for the tool result before requesting another.\n\
         - Do not invent tool results. Do not output tool_result blocks yourself.\n\
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

    if let Some(call) = envelope.tool_call {
        let id = format!("call_{}", new_id());
        response.tool_calls = vec![ToolCall {
            id,
            name: call.name,
            input: call.input,
        }];
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

/// Spawn `command` with piped stdio and enforce a wall-clock timeout.
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
/// The fix: read stdout/stderr on dedicated drainer threads, take ownership
/// of those handles, and on timeout `child.kill()` + `child.wait()` only.
/// We deliberately do NOT join the drainer threads when we time out — they
/// stay parked on the grandchild's open write-end, but that's a thread leak,
/// not a deadlock of the main bench loop, and the next call to this function
/// is unaffected. On the happy path the child exits, the drainers finish on
/// their own, and we join them to recover the captured output.
fn run_with_timeout(mut command: Command, timeout: Duration) -> KimetsuResult<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_handle = stdout.map(|mut s| {
        thread::spawn(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf)?;
            Ok(buf)
        })
    });
    let stderr_handle = stderr.map(|mut s| {
        thread::spawn(move || -> std::io::Result<Vec<u8>> {
            use std::io::Read;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf)?;
            Ok(buf)
        })
    });

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

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            // Drainer threads may still be parked on a grandchild's pipe;
            // we intentionally do not join them. Build the error from
            // whatever the timeout said, not from the (unreadable) pipes.
            return Err(format!(
                "timed out after {}s; child killed; grandchild may still hold stdio pipes",
                timeout.as_secs()
            )
            .into());
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn parse_claude_code_output(stdout: &[u8]) -> KimetsuResult<ModelResponse> {
    let text = String::from_utf8_lossy(stdout);
    let response: ClaudeCodeJson = serde_json::from_str(&text).map_err(|err| {
        format!(
            "failed to parse claude_code JSON output: {err}; output: {}",
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
        },
    })
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
    result: Option<String>,
    stop_reason: Option<String>,
    total_cost_usd: Option<f32>,
    usage: Option<ClaudeCodeUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct ClaudeCodeUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
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
}
