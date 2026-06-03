use std::path::Path;
use std::time::Duration;

use kimetsu_core::KimetsuResult;
use kimetsu_core::config::ProjectConfig;
use kimetsu_core::env_file::resolve_env_value;
use kimetsu_core::secret::SecretString;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::{
    MessageContent, MessageRole, ModelProvider, ModelRequest, ModelResponse, StopReason,
    TokenUsage, ToolCall, ToolChoice,
};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    // v0.4.9: SecretString — same rationale as ClaudeCodeProvider.
    // Debug / Display / serde all emit "[REDACTED]"; cleartext
    // only flows out via expose_secret() at the HTTP header
    // injection below.
    api_key: SecretString,
    model: String,
    /// When set, requests POST to `<base_url>/v1/messages` (Anthropic-
    /// compatible endpoints such as a LiteLLM proxy). When `None`, the
    /// default Anthropic API URL is used.
    base_url: Option<String>,
}

impl AnthropicProvider {
    pub fn from_config(repo_root: &Path, config: &ProjectConfig) -> KimetsuResult<Option<Self>> {
        Self::from_config_with_key(repo_root, config, None)
    }

    pub fn from_config_with_key(
        repo_root: &Path,
        config: &ProjectConfig,
        api_key_override: Option<&str>,
    ) -> KimetsuResult<Option<Self>> {
        if config.model.provider != "anthropic" {
            return Err(format!("unsupported model provider: {}", config.model.provider).into());
        }

        let api_key = api_key_override
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| resolve_env_value(repo_root, &config.model.api_key_env));
        let Some(api_key) = api_key else {
            return Ok(None);
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(config.model.request_timeout_secs))
            .build()?;

        Ok(Some(Self {
            client,
            api_key: SecretString::new(api_key),
            model: config.model.model.clone(),
            base_url: None,
        }))
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Build a provider directly from resolved values — used by the
    /// SessionEnd distiller, which controls model/key/base independent of
    /// the project's `[model]` section. `base_url` is normalized: empty/
    /// whitespace becomes `None`.
    pub fn for_distiller(
        model: impl Into<String>,
        api_key: impl Into<String>,
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> KimetsuResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()?;
        Ok(Self {
            client,
            api_key: SecretString::new(api_key.into()),
            model: model.into(),
            base_url: base_url.filter(|value| !value.trim().is_empty()),
        })
    }
}

/// Resolve the messages endpoint: `<base>/v1/messages` when a base URL is
/// configured, else the default Anthropic API URL.
fn messages_url(base_url: &Option<String>) -> String {
    match base_url {
        Some(base) => format!("{}/v1/messages", base.trim_end_matches('/')),
        None => MESSAGES_URL.to_string(),
    }
}

impl ModelProvider for AnthropicProvider {
    fn complete(&mut self, request: ModelRequest) -> KimetsuResult<ModelResponse> {
        let body = build_request_body(&self.model, &request);
        let url = messages_url(&self.base_url);
        let response = self
            .client
            .post(&url)
            // v0.4.9: explicit cleartext leak point for the HTTP
            // header. The reqwest client never logs header values
            // by default; if a future caller adds request logging
            // this is the line to add redaction at.
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()?;

        let status = response.status();
        let response_text = response.text()?;
        if !status.is_success() {
            return Err(format!(
                "anthropic request failed ({status}): {}",
                response_error_summary(&response_text)
            )
            .into());
        }

        parse_response(&response_text)
    }
}

fn build_request_body(model: &str, request: &ModelRequest) -> Value {
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();

    for message in &request.messages {
        if matches!(message.role, MessageRole::System) {
            let system_text = message.text_content();
            if !system_text.trim().is_empty() {
                system_parts.push(system_text);
            }
            continue;
        }

        let role = match message.role {
            MessageRole::Assistant => "assistant",
            MessageRole::User | MessageRole::Tool => "user",
            MessageRole::System => unreachable!(),
        };
        let content = message
            .content
            .iter()
            .filter_map(map_content_block)
            .collect::<Vec<_>>();
        if !content.is_empty() {
            messages.push(json!({
                "role": role,
                "content": content,
            }));
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": request.max_output_tokens,
        "temperature": request.temperature,
        "messages": messages,
    });

    if !system_parts.is_empty() {
        body["system"] = json!(system_parts.join("\n\n"));
    }

    if !request.tools.is_empty() && !matches!(request.tool_choice, ToolChoice::None) {
        body["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": &tool.name,
                    "description": &tool.description,
                    "input_schema": &tool.input_schema,
                }))
                .collect::<Vec<_>>()
        );
        body["tool_choice"] = match request.tool_choice {
            ToolChoice::Auto => json!({ "type": "auto" }),
            ToolChoice::Required => json!({ "type": "any" }),
            ToolChoice::None => Value::Null,
        };
    }

    body
}

fn map_content_block(content: &MessageContent) -> Option<Value> {
    match content {
        MessageContent::Text { text } => {
            if text.trim().is_empty() {
                None
            } else {
                Some(json!({ "type": "text", "text": text }))
            }
        }
        MessageContent::ToolCall { id, name, input } => Some(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        MessageContent::ToolResult {
            tool_call_id,
            output,
            ..
        } => Some(json!({
            "type": "tool_result",
            "tool_use_id": tool_call_id,
            "content": serde_json::to_string(output).unwrap_or_else(|_| output.to_string()),
        })),
    }
}

fn parse_response(response_text: &str) -> KimetsuResult<ModelResponse> {
    let response: AnthropicResponse = serde_json::from_str(response_text)?;
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in response.content {
        match block {
            AnthropicContent::Text { text } => text_parts.push(text),
            AnthropicContent::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall { id, name, input });
            }
            AnthropicContent::Other => {}
        }
    }

    let stop_reason = if !tool_calls.is_empty() {
        StopReason::ToolUse
    } else {
        match response.stop_reason.as_deref() {
            Some("end_turn") | None => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            _ => StopReason::Error,
        }
    };

    Ok(ModelResponse {
        text: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        },
        tool_calls,
        stop_reason,
        usage: response.usage.unwrap_or_default().into(),
    })
}

fn response_error_summary(response_text: &str) -> String {
    let parsed = serde_json::from_str::<Value>(response_text).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or(response_text);
    truncate(message, 700)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    stop_reason: Option<String>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
    /// v0.3.4a: Anthropic returns 0 (not absent) when no cache write
    /// happened on this call, but we accept absence too via Option +
    /// `unwrap_or_default` for forward-compat with older API responses.
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

impl From<AnthropicUsage> for TokenUsage {
    fn from(value: AnthropicUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cost_usd: 0.0,
            // v0.3.4a: surface cache stats so callers get the same
            // visibility as the claude_code provider.
            cache_creation_input_tokens: value.cache_creation_input_tokens.unwrap_or_default(),
            cache_read_input_tokens: value.cache_read_input_tokens.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelMessage, ToolDefinition};

    /// v0.4.9 regression guard. Mirror of the ClaudeCodeProvider
    /// test — `#[derive(Debug)]` must not leak `api_key`.
    #[test]
    fn debug_format_does_not_leak_api_key() {
        let token = "sk-ant-api03-DEFINITELY-LEAKED-IF-BROKEN-1234567890";
        let provider = AnthropicProvider {
            client: Client::new(),
            api_key: SecretString::new(token),
            model: "claude-opus-4-7".into(),
            base_url: None,
        };
        let dbg = format!("{:?}", provider);
        assert!(
            !dbg.contains("DEFINITELY-LEAKED-IF-BROKEN"),
            "Debug-print MUST NOT include the inner token: {dbg}"
        );
        assert!(dbg.contains("REDACTED"));
        assert!(dbg.contains("claude-opus-4-7"));
    }

    #[test]
    fn messages_url_uses_base_when_set() {
        assert_eq!(messages_url(&None), MESSAGES_URL);
        assert_eq!(
            messages_url(&Some("http://localhost:4000".to_string())),
            "http://localhost:4000/v1/messages"
        );
        assert_eq!(
            messages_url(&Some("http://localhost:4000/".to_string())),
            "http://localhost:4000/v1/messages"
        );
    }

    #[test]
    fn for_distiller_builds_provider_with_base_url() {
        let p = AnthropicProvider::for_distiller(
            "claude-haiku-4-5",
            "sk-test",
            Some("http://localhost:4000".to_string()),
            60,
        )
        .expect("build");
        assert_eq!(p.model_name(), "claude-haiku-4-5");
        assert_eq!(p.base_url.as_deref(), Some("http://localhost:4000"));
    }

    #[test]
    fn request_maps_system_and_tool_blocks_to_anthropic_shape() {
        let request = ModelRequest {
            messages: vec![
                ModelMessage {
                    role: MessageRole::System,
                    content: vec![MessageContent::Text {
                        text: "Use strict JSON.".to_string(),
                    }],
                },
                ModelMessage::user_text("Plan the change."),
                ModelMessage::assistant_tool_calls(vec![ToolCall {
                    id: "toolu_1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({ "path": "src/lib.rs" }),
                }]),
                ModelMessage::tool_result(
                    "toolu_1",
                    "read_file",
                    json!({ "ok": true, "output": "pub fn x() {}" }),
                ),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file.".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: 1024,
            temperature: 0.2,
            metadata: Value::Null,
        };

        let body = build_request_body("claude-opus-4-7", &request);
        assert_eq!(body["system"], "Use strict JSON.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["name"], "read_file");
    }

    #[test]
    fn response_maps_text_tool_use_and_usage() {
        let response = parse_response(
            r#"{
                "content": [
                    {"type": "text", "text": "Reading file."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "src/lib.rs"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }"#,
        )
        .expect("parse response");

        assert_eq!(response.text.as_deref(), Some("Reading file."));
        assert_eq!(response.tool_calls.len(), 1);
        assert!(matches!(response.stop_reason, StopReason::ToolUse));
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 5);
    }
}
