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

use crate::model::{
    MessageContent, MessageRole, ModelProvider, ModelRequest, ModelResponse, StopReason,
    TokenUsage, ToolChoice,
};

#[derive(Debug, Clone)]
pub struct ClaudeCodeProvider {
    auth: ClaudeCodeAuth,
    model: String,
    timeout: Duration,
    max_budget_usd: f32,
}

#[derive(Debug, Clone)]
struct ClaudeCodeAuth {
    kind: ClaudeCodeAuthKind,
    secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeCodeAuthKind {
    OAuth,
    AnthropicApiKey,
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
            auth: ClaudeCodeAuth {
                kind: auth_kind_for_env(&config.model.api_key_env),
                secret,
            },
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
        if !request.tools.is_empty() || !matches!(request.tool_choice, ToolChoice::None) {
            return Err(
                "claude_code provider v0.1 only supports text-only model calls without tools"
                    .into(),
            );
        }

        let (system_prompt, prompt) = render_request_for_claude_code(&request)?;
        let work_dir = TempCommandDir::create()?;
        let config_dir = work_dir.path().join("config");
        fs::create_dir_all(&config_dir)?;

        let mut command = Command::new("claude");
        command
            .current_dir(work_dir.path())
            .env("CLAUDE_CONFIG_DIR", &config_dir)
            .env("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1")
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
            .arg("--system-prompt")
            .arg(&system_prompt);

        match self.auth.kind {
            ClaudeCodeAuthKind::OAuth => {
                command
                    .env("CLAUDE_CODE_OAUTH_TOKEN", &self.auth.secret)
                    .env_remove("ANTHROPIC_API_KEY");
            }
            ClaudeCodeAuthKind::AnthropicApiKey => {
                command
                    .env("ANTHROPIC_API_KEY", &self.auth.secret)
                    .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
                    .arg("--bare");
            }
        }

        let output = run_with_timeout(command, self.timeout).map_err(|err| {
            redact_token(
                &format!("claude_code provider failed: {err}"),
                &self.auth.secret,
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
                &self.auth.secret,
            )
            .into());
        }

        parse_claude_code_output(&output.stdout)
    }
}

fn auth_kind_for_env(env_name: &str) -> ClaudeCodeAuthKind {
    if env_name == "ANTHROPIC_API_KEY" {
        ClaudeCodeAuthKind::AnthropicApiKey
    } else {
        ClaudeCodeAuthKind::OAuth
    }
}

fn render_request_for_claude_code(request: &ModelRequest) -> KimetsuResult<(String, String)> {
    let mut system_parts = Vec::new();
    let mut prompt_parts = Vec::new();

    for message in &request.messages {
        let text = text_only_message(&message.role, &message.content)?;
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

fn text_only_message(role: &MessageRole, content: &[MessageContent]) -> KimetsuResult<String> {
    let mut parts = Vec::new();
    for block in content {
        match block {
            MessageContent::Text { text } => parts.push(text.as_str()),
            MessageContent::ToolCall { .. } | MessageContent::ToolResult { .. } => {
                return Err(format!(
                    "claude_code provider cannot render non-text {role:?} message content"
                )
                .into());
            }
        }
    }
    Ok(parts.join("\n"))
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> KimetsuResult<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }

        if started.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Err(format!(
                "timed out after {}s; {}",
                timeout.as_secs(),
                output_summary(&output)
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
    use crate::model::{ModelMessage, ToolDefinition};
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
    fn rejects_tool_config() {
        let mut provider = ClaudeCodeProvider {
            auth: ClaudeCodeAuth {
                kind: ClaudeCodeAuthKind::OAuth,
                secret: "secret".to_string(),
            },
            model: "claude-opus-4-7".to_string(),
            timeout: Duration::from_secs(1),
            max_budget_usd: 1.0,
        };
        let request = ModelRequest {
            messages: vec![ModelMessage::user_text("hello")],
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read file.".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 1000,
            temperature: 0.2,
            metadata: json!(null),
        };

        let err = provider
            .complete(request)
            .expect_err("tool config should be rejected");
        assert!(err.to_string().contains("text-only"));
    }

    #[test]
    fn anthropic_api_key_env_uses_bare_auth_kind() {
        assert_eq!(
            auth_kind_for_env("ANTHROPIC_API_KEY"),
            ClaudeCodeAuthKind::AnthropicApiKey
        );
        assert_eq!(
            auth_kind_for_env("CLAUDE_CODE_OAUTH_TOKEN"),
            ClaudeCodeAuthKind::OAuth
        );
    }
}
